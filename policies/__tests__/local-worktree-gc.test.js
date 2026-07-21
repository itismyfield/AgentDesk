const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { loadRoutine } = require("./support/routine-harness");

const REPO_ROOT = path.resolve(__dirname, "..", "..");
const helper = require(path.join(REPO_ROOT, "routines/monitoring/local_worktree_inventory.js"));

const DAY = 24 * 60 * 60;

// --- Pure classification: the #4595 data-loss invariant ---

test("dirty worktrees are always PRESERVE (uncommitted work protected)", () => {
  const d = helper.classifyWorktree({
    registered: true,
    locked: false,
    dirty: true,
    merged: true, // even if merged, dirty wins
    age_seconds: 30 * DAY,
    worktree_state: "dirty",
  });
  assert.equal(d.disposition, "PRESERVE");
  assert.equal(d.positive_ownership_proof, false);
});

test("locked worktrees are always PRESERVE", () => {
  const d = helper.classifyWorktree({
    registered: true,
    locked: true,
    dirty: false,
    merged: true,
    age_seconds: 90 * DAY,
    worktree_state: "clean",
  });
  assert.equal(d.disposition, "PRESERVE");
});

test("unknown inspection state never authorizes cleanup", () => {
  const d = helper.classifyWorktree({
    registered: true,
    locked: false,
    dirty: false,
    merged: null,
    age_seconds: 90 * DAY,
    worktree_state: "unknown",
  });
  assert.equal(d.disposition, "PRESERVE");
});

test("clean + unmerged + recent stays PRESERVE (possible live work)", () => {
  const d = helper.classifyWorktree({
    registered: true,
    locked: false,
    dirty: false,
    merged: false,
    age_seconds: 2 * DAY,
    worktree_state: "clean",
  });
  assert.equal(d.disposition, "PRESERVE");
});

test("clean + unmerged + aged is AGED_ORPHAN_REVIEW, not deletion", () => {
  const d = helper.classifyWorktree({
    registered: true,
    locked: false,
    dirty: false,
    merged: false,
    age_seconds: 30 * DAY,
    worktree_state: "clean",
  });
  assert.equal(d.disposition, "AGED_ORPHAN_REVIEW");
  assert.equal(d.positive_ownership_proof, false);
});

test("clean + merged + registered is SAFE_MERGED_CANDIDATE (report-only)", () => {
  const d = helper.classifyWorktree({
    registered: true,
    locked: false,
    dirty: false,
    merged: true,
    age_seconds: 30 * DAY,
    worktree_state: "clean",
  });
  assert.equal(d.disposition, "SAFE_MERGED_CANDIDATE");
  assert.equal(d.positive_ownership_proof, false);
});

// --- Full inventory over a real temp fixture with injected read-only deps ---

function makeFixture() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "wt-inv-"));
  const worktreesRoot = path.join(root, ".claude", "worktrees");
  fs.mkdirSync(worktreesRoot, { recursive: true });

  const now = Date.parse("2026-07-21T00:00:00Z");
  const old = now - 30 * DAY * 1000;
  const recent = now - 1 * DAY * 1000;

  const mk = (name, mtimeMs) => {
    const p = path.join(worktreesRoot, name);
    fs.mkdirSync(p);
    fs.utimesSync(p, new Date(mtimeMs), new Date(mtimeMs));
    return p;
  };

  const dirtyP = mk("agent-dirty", old);
  const lockedP = mk("agent-locked", old);
  const mergedP = mk("agent-merged", old);
  const unmergedAgedP = mk("agent-unmerged-aged", old);
  const unmergedRecentP = mk("agent-live", recent);
  // A non-agent dir that must be ignored entirely.
  mk("release-main", old);

  const heads = {
    [dirtyP]: "aaa",
    [lockedP]: "bbb",
    [mergedP]: "ccc",
    [unmergedAgedP]: "ddd",
    [unmergedRecentP]: "eee",
  };
  const registered = {};
  for (const [p, head] of Object.entries(heads)) {
    registered[p] = { path: p, head, branch: `refs/heads/${path.basename(p)}`, locked: p === lockedP };
  }
  // Registered-but-missing agent worktree (git knows it, directory gone).
  const missingP = path.join(worktreesRoot, "agent-missing");
  registered[missingP] = { path: missingP, head: "fff", branch: "refs/heads/gone", locked: false };

  const deps = {
    worktreeList: () => registered,
    statusPorcelain: (p) => (p === dirtyP ? " M file.txt\n" : ""),
    isMerged: (head) => head === "ccc", // only agent-merged is merged
    sizeKb: (p) =>
      ({ [dirtyP]: 100, [lockedP]: 200, [mergedP]: 300, [unmergedAgedP]: 400, [unmergedRecentP]: 500 }[p] || 10),
  };

  return { root, worktreesRoot, now, deps, paths: { dirtyP, lockedP, mergedP, unmergedAgedP, unmergedRecentP, missingP } };
}

test("runInventory classifies a mixed fixture correctly and validates schema", () => {
  const fx = makeFixture();
  try {
    const report = helper.runInventory({
      repoDir: fx.root,
      worktreesRoot: fx.worktreesRoot,
      nowMs: fx.now,
      deps: fx.deps,
      agedOrphanSeconds: 7 * DAY,
    });

    // Report-only, safe-by-construction invariants.
    assert.equal(report.mode, "report_only");
    assert.equal(report.destructive_actions, 0);
    assert.equal(report.schema_version, helper.SCHEMA_VERSION);

    const by = Object.fromEntries(report.worktrees.map((w) => [w.name, w]));

    // Non-agent directory excluded.
    assert.ok(!by["release-main"], "non-agent dirs must be excluded");

    assert.equal(by["agent-dirty"].disposition, "PRESERVE");
    assert.equal(by["agent-dirty"].dirty, true);
    assert.equal(by["agent-locked"].disposition, "PRESERVE");
    assert.equal(by["agent-locked"].locked, true);
    assert.equal(by["agent-live"].disposition, "PRESERVE"); // unmerged + recent
    assert.equal(by["agent-unmerged-aged"].disposition, "AGED_ORPHAN_REVIEW");
    assert.equal(by["agent-merged"].disposition, "SAFE_MERGED_CANDIDATE");

    // Registered-but-missing directory surfaces and is preserved.
    assert.equal(by["agent-missing"].worktree_state, "missing");
    assert.equal(by["agent-missing"].disposition, "PRESERVE");

    // Sizes are captured so the 70GB problem is visible; totals aggregate them.
    assert.equal(by["agent-merged"].size_kb, 300);
    assert.equal(report.totals.total_size_kb, 100 + 200 + 300 + 400 + 500);
    assert.equal(report.totals.count, 6); // 5 present agent-* + 1 missing

    // Every entry carries no ownership proof.
    for (const w of report.worktrees) assert.equal(w.positive_ownership_proof, false);
  } finally {
    fs.rmSync(fx.root, { recursive: true, force: true });
  }
});

test("validateReport rejects a dirty worktree marked for anything but PRESERVE", () => {
  const bad = {
    schema_version: helper.SCHEMA_VERSION,
    mode: "report_only",
    destructive_actions: 0,
    totals: {},
    inspection_errors: [],
    worktrees: [
      {
        path: "/x/agent-bad",
        disposition: "SAFE_MERGED_CANDIDATE",
        worktree_state: "dirty",
        dirty: true,
        locked: false,
        positive_ownership_proof: false,
        age_seconds: 1,
        size_kb: 1,
      },
    ],
  };
  assert.throws(() => helper.validateReport(bad), /must be PRESERVE/);
});

// --- Safety by construction: the helper module has no destructive code path ---

test("inventory helper source contains zero destructive operations", () => {
  const raw = fs.readFileSync(path.join(REPO_ROOT, "routines/monitoring/local_worktree_inventory.js"), "utf8");
  // Scan executable code only: strip block and line comments so prose that
  // merely names a destructive command (e.g. explaining what is NOT done) does
  // not trip the guard. The guarantee is that no destructive call is reachable.
  const src = raw.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
  const forbidden = [
    /\brm\b[^"']*-r?f/i,
    /\bfind\b[^"']*-delete/i,
    /worktree\s+remove/i,
    /worktree\s+prune/i,
    /branch\s+-D/i,
    /update-ref\s+-d/i,
    /\.rmSync\b/,
    /\.rmdirSync\b/,
    /\.unlinkSync\b/,
  ];
  for (const pat of forbidden) {
    assert.doesNotMatch(src, pat, `helper must not contain ${pat}`);
  }
});

// --- The QuickJS routine is a schedule/dispatch shim only ---

test("routine dispatches the deterministic helper and self-guards per day", () => {
  const { routine, tick } = loadRoutine("routines/local-worktree-gc.js");
  assert.equal(routine.name, "Local agent worktree inventory");

  const first = tick({ now: new Date("2026-07-21T00:00:00Z"), checkpoint: null });
  assert.equal(first.action, "agent");
  assert.match(first.prompt, /local_worktree_inventory\.js/);
  assert.match(first.prompt, /report-only inventory/i);
  assert.match(first.prompt, /Do NOT remove, prune, or modify/);

  // Same KST day => no duplicate dispatch.
  const second = tick({ now: new Date("2026-07-21T05:00:00Z"), checkpoint: first.checkpoint });
  assert.equal(second.action, "complete");
});

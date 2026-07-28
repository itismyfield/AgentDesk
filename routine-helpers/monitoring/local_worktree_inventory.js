// Local agent worktree inventory helper (#4684)
//
// Deterministic, READ-ONLY inventory of `.claude/worktrees/agent-*` git
// worktrees. QuickJS routines have no filesystem bridge, so the scheduled
// routine (`routines/local-worktree-gc.js`) dispatches one agent turn that runs
// this Node helper and returns its JSON stdout verbatim.
//
// SAFETY BY CONSTRUCTION: this module performs zero destructive operations. It
// never removes worktrees, prunes refs, deletes branches, or unlinks files.
// Every child-process call is a read-only git/du subcommand. There is no code
// path that deletes anything — "never deletes" is provable by inspection, not by
// trusting an LLM to obey natural-language instructions. Disposition labels are
// advisory classifications for a human/future prune step; this helper only reads
// and emits a schema-validated report. The #4595 lesson is enforced here:
// dirty, locked, unmerged, and unknown worktrees are always marked PRESERVE, so
// the exact uncommitted work a naive GC could destroy is protected.

const fs = require("node:fs");
const path = require("node:path");
const { execFileSync } = require("node:child_process");

const SCHEMA_VERSION = 1;
const DEFAULT_AGED_ORPHAN_SECONDS = 7 * 24 * 60 * 60; // 7 days (issue proposal #2)
const AGENT_WORKTREE_PREFIX = "agent-";

// --- Read-only signal collectors (dependency-injected for tests) ---

// Parse `git worktree list --porcelain` into { <abs path>: {head, branch, locked, bare} }.
function parseWorktreeList(porcelain) {
  const registered = {};
  let current = null;
  for (const rawLine of String(porcelain).split("\n")) {
    const line = rawLine.trimEnd();
    if (line.startsWith("worktree ")) {
      current = { path: line.slice("worktree ".length), head: null, branch: null, locked: false };
      registered[current.path] = current;
    } else if (!current) {
      continue;
    } else if (line.startsWith("HEAD ")) {
      current.head = line.slice("HEAD ".length);
    } else if (line.startsWith("branch ")) {
      current.branch = line.slice("branch ".length);
    } else if (line === "locked" || line.startsWith("locked ")) {
      current.locked = true;
    } else if (line === "") {
      current = null;
    }
  }
  return registered;
}

function defaultDeps(repoDir) {
  const gitRO = (args) =>
    execFileSync("git", ["-C", repoDir, ...args], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      maxBuffer: 32 * 1024 * 1024,
    });
  return {
    // Read-only: list registered worktrees.
    worktreeList: () => parseWorktreeList(gitRO(["worktree", "list", "--porcelain"])),
    // Read-only: porcelain status of a specific worktree; non-empty => dirty.
    statusPorcelain: (wtPath) =>
      execFileSync("git", ["-C", wtPath, "status", "--porcelain"], {
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
        maxBuffer: 32 * 1024 * 1024,
      }),
    // Read-only: is <head> an ancestor of the integration ref? merge-base exits 0/1.
    isMerged: (head, baseRef) => {
      try {
        execFileSync("git", ["-C", repoDir, "merge-base", "--is-ancestor", head, baseRef], {
          stdio: "ignore",
        });
        return true;
      } catch {
        return false;
      }
    },
    // Read-only: apparent-size disk usage in KiB. `du` mutates nothing.
    sizeKb: (wtPath) => {
      const out = execFileSync("du", ["-sk", wtPath], {
        encoding: "utf8",
        stdio: ["ignore", "pipe", "ignore"],
      });
      const kb = parseInt(String(out).trim().split(/\s+/)[0], 10);
      return Number.isFinite(kb) ? kb : null;
    },
  };
}

// --- Pure classification (no I/O) ---

// Given objective signals, decide an advisory disposition. NEVER a deletion
// authority: positive_ownership_proof is always false and destructive_actions
// stays 0. Dirty/locked/unmerged-recent/unknown => PRESERVE (the #4595 lesson).
function classifyWorktree(signals, opts = {}) {
  const agedSeconds =
    typeof opts.agedOrphanSeconds === "number" ? opts.agedOrphanSeconds : DEFAULT_AGED_ORPHAN_SECONDS;
  const base = { positive_ownership_proof: false };

  if (signals.worktree_state === "unknown" || signals.locked) {
    return { ...base, disposition: "PRESERVE", reason: signals.locked ? "locked worktree" : "inspection unknown" };
  }
  if (signals.worktree_state === "missing") {
    // Registered but directory absent: a `git worktree prune` concern, never ours to delete.
    return { ...base, disposition: "PRESERVE", reason: "registered worktree with missing directory" };
  }
  if (signals.dirty) {
    return { ...base, disposition: "PRESERVE", reason: "uncommitted changes present" };
  }
  if (signals.merged === false) {
    const aged = typeof signals.age_seconds === "number" && signals.age_seconds > agedSeconds;
    if (aged) {
      // Proposal #2: aged unmerged orphan. Flag for human review + archive-ref
      // backup BEFORE any future removal. This helper never removes it.
      return {
        ...base,
        disposition: "AGED_ORPHAN_REVIEW",
        reason: `clean but unmerged, mtime older than ${agedSeconds}s; archive branch tip before any prune`,
      };
    }
    return { ...base, disposition: "PRESERVE", reason: "clean but unmerged and recent (possibly live work)" };
  }
  if (signals.merged === true && signals.registered && !signals.dirty) {
    // Proposal #1: clean AND merged AND registered => the session-verified safe
    // reclaim condition. Still report-only; surfaced as a candidate, not deleted.
    return { ...base, disposition: "SAFE_MERGED_CANDIDATE", reason: "clean and merged into integration ref" };
  }
  return { ...base, disposition: "PRESERVE", reason: "no positive ownership proof" };
}

// --- Enumeration + report assembly ---

function listAgentWorktreeDirs(worktreesRoot) {
  let entries;
  try {
    entries = fs.readdirSync(worktreesRoot, { withFileTypes: true });
  } catch (err) {
    if (err && err.code === "ENOENT") return [];
    throw err;
  }
  const dirs = [];
  for (const entry of entries) {
    if (!entry.name.startsWith(AGENT_WORKTREE_PREFIX)) continue;
    const abs = path.join(worktreesRoot, entry.name);
    // Do not follow symlinks: lstat, and only accept real directories.
    let st;
    try {
      st = fs.lstatSync(abs);
    } catch {
      continue;
    }
    if (st.isSymbolicLink() || !st.isDirectory()) continue;
    dirs.push({ name: entry.name, path: abs, mtimeMs: st.mtimeMs });
  }
  return dirs;
}

function runInventory(options = {}) {
  const repoDir = options.repoDir || process.env.AGENTDESK_REPO_DIR || process.cwd();
  const worktreesRoot = options.worktreesRoot || path.join(repoDir, ".claude", "worktrees");
  const baseRef = options.baseRef || "origin/main";
  const nowMs = typeof options.nowMs === "number" ? options.nowMs : Date.now();
  const agedOrphanSeconds =
    typeof options.agedOrphanSeconds === "number" ? options.agedOrphanSeconds : DEFAULT_AGED_ORPHAN_SECONDS;
  const deps = options.deps || defaultDeps(repoDir);

  const inspectionErrors = [];
  let registered = {};
  try {
    registered = deps.worktreeList() || {};
  } catch (err) {
    inspectionErrors.push({ path: repoDir, error: `worktree list failed: ${err.message || err}` });
  }

  const dirs = listAgentWorktreeDirs(worktreesRoot);
  const seen = new Set();
  const worktrees = [];

  const collect = (name, absPath, mtimeMs) => {
    seen.add(absPath);
    const reg = registered[absPath];
    const isRegistered = Boolean(reg);
    const locked = Boolean(reg && reg.locked);
    const head = reg ? reg.head : null;
    const branch = reg ? reg.branch : null;

    let worktreeState = "unknown";
    let dirty = null;
    let merged = null;
    const dirExists = mtimeMs !== null;

    if (!dirExists) {
      worktreeState = "missing";
    } else {
      try {
        const status = deps.statusPorcelain(absPath);
        dirty = String(status).trim().length > 0;
        worktreeState = dirty ? "dirty" : "clean";
      } catch (err) {
        worktreeState = "unknown";
        inspectionErrors.push({ path: absPath, error: `status failed: ${err.message || err}` });
      }
      if (head && worktreeState !== "unknown") {
        try {
          merged = deps.isMerged(head, baseRef);
        } catch (err) {
          merged = null;
          inspectionErrors.push({ path: absPath, error: `merge-base failed: ${err.message || err}` });
        }
      }
    }

    let sizeKb = null;
    if (dirExists) {
      try {
        sizeKb = deps.sizeKb(absPath);
      } catch (err) {
        inspectionErrors.push({ path: absPath, error: `size failed: ${err.message || err}` });
      }
    }

    const ageSeconds = dirExists ? Math.max(0, Math.floor((nowMs - mtimeMs) / 1000)) : null;
    const signals = {
      registered: isRegistered,
      locked,
      dirty: dirty === true,
      merged,
      age_seconds: ageSeconds,
      worktree_state: worktreeState,
    };
    const decision = classifyWorktree(signals, { agedOrphanSeconds });

    worktrees.push({
      path: absPath,
      name,
      age_seconds: ageSeconds,
      size_kb: sizeKb,
      registered: isRegistered,
      locked,
      dirty,
      merged,
      head,
      branch,
      worktree_state: worktreeState,
      disposition: decision.disposition,
      positive_ownership_proof: false,
      reason: decision.reason,
    });
  };

  for (const dir of dirs) {
    collect(dir.name, dir.path, dir.mtimeMs);
  }
  // Registered agent-* worktrees whose directory is missing must still surface.
  for (const [absPath, reg] of Object.entries(registered)) {
    if (seen.has(absPath)) continue;
    if (!path.basename(absPath).startsWith(AGENT_WORKTREE_PREFIX)) continue;
    collect(path.basename(absPath), absPath, null);
    void reg;
  }

  worktrees.sort((a, b) => (b.size_kb || 0) - (a.size_kb || 0));

  const totalSizeKb = worktrees.reduce((sum, w) => sum + (w.size_kb || 0), 0);
  const orphanCount = worktrees.filter(
    (w) => w.disposition === "AGED_ORPHAN_REVIEW" || w.disposition === "SAFE_MERGED_CANDIDATE",
  ).length;
  const preserveCount = worktrees.filter((w) => w.disposition === "PRESERVE").length;

  const report = {
    schema_version: SCHEMA_VERSION,
    generated_at: new Date(nowMs).toISOString(),
    root: worktreesRoot,
    base_ref: baseRef,
    mode: "report_only",
    destructive_actions: 0,
    aged_orphan_seconds: agedOrphanSeconds,
    totals: {
      count: worktrees.length,
      total_size_kb: totalSizeKb,
      orphan_count: orphanCount,
      preserve_count: preserveCount,
    },
    worktrees,
    inspection_errors: inspectionErrors,
  };
  validateReport(report);
  return report;
}

// --- Schema validation (throws on violation) ---

const VALID_DISPOSITIONS = new Set(["PRESERVE", "SAFE_MERGED_CANDIDATE", "AGED_ORPHAN_REVIEW"]);
const VALID_STATES = new Set(["clean", "dirty", "missing", "unknown"]);

function validateReport(report) {
  const fail = (msg) => {
    throw new Error(`local-worktree-inventory schema violation: ${msg}`);
  };
  if (!report || typeof report !== "object") fail("report is not an object");
  if (report.schema_version !== SCHEMA_VERSION) fail("schema_version mismatch");
  if (report.mode !== "report_only") fail("mode must be report_only");
  if (report.destructive_actions !== 0) fail("destructive_actions must be 0");
  if (!Array.isArray(report.worktrees)) fail("worktrees must be an array");
  if (!Array.isArray(report.inspection_errors)) fail("inspection_errors must be an array");
  if (!report.totals || typeof report.totals !== "object") fail("totals must be an object");
  for (const w of report.worktrees) {
    if (typeof w.path !== "string" || !w.path) fail("worktree.path must be a non-empty string");
    if (!VALID_DISPOSITIONS.has(w.disposition)) fail(`invalid disposition '${w.disposition}' for ${w.path}`);
    if (!VALID_STATES.has(w.worktree_state)) fail(`invalid worktree_state '${w.worktree_state}' for ${w.path}`);
    if (w.positive_ownership_proof !== false) fail(`positive_ownership_proof must be false for ${w.path}`);
    if (!(w.age_seconds === null || typeof w.age_seconds === "number")) fail("age_seconds must be number|null");
    if (!(w.size_kb === null || typeof w.size_kb === "number")) fail("size_kb must be number|null");
    // Data-loss invariant: anything dirty or locked MUST be PRESERVE.
    if ((w.dirty === true || w.locked === true) && w.disposition !== "PRESERVE") {
      fail(`dirty/locked worktree ${w.path} must be PRESERVE, got ${w.disposition}`);
    }
  }
  return report;
}

function main() {
  const report = runInventory();
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
}

if (require.main === module) {
  main();
}

module.exports = {
  SCHEMA_VERSION,
  DEFAULT_AGED_ORPHAN_SECONDS,
  parseWorktreeList,
  classifyWorktree,
  listAgentWorktreeDirs,
  runInventory,
  validateReport,
};

/** @module policies/lib/kanban-inventory-refresh
 *
 * #1078: Extracted from kanban-rules.js as part of the policy modularization pass.
 *
 * Owns the post-dispatch "auto-refresh generated inventory docs" pipeline:
 *   - INVENTORY_DOC_PATHS — canonical list of generated doc paths
 *   - _extractRepoFromUrl — used by preflight too
 *   - _firstPresent / _execOrThrow / _splitNonEmptyLines / _normalizeDispatchTimestamp
 *   - _resolveCompletedWorktreePath / _resolveCompletedBranch
 *   - _dispatchTouchedSrcSinceCreated / _inventoryDocsChanged
 *   - _autoRefreshInventoryDocs — main entry called from OnDispatchCompleted
 *
 * Depends on the global `agentdesk.*` surface (exec, log, runtime) and on
 * `notifyCardOwner` from `./kanban-notifications` for escalation on failure.
 */

var _notifications = require("./kanban-notifications");
var notifyCardOwner = _notifications.notifyCardOwner;

var INVENTORY_DOC_PATHS = [
  "docs/generated/module-inventory.md",
  "docs/generated/route-inventory.md",
  "docs/generated/worker-inventory.md"
];

function _extractRepoFromUrl(url) {
  if (!url) return null;
  var match = url.match(/github\.com\/([^\/]+\/[^\/]+)/);
  return match ? match[1] : null;
}

function _firstPresent() {
  for (var i = 0; i < arguments.length; i++) {
    var value = arguments[i];
    if (typeof value === "string" && value.trim() !== "") return value;
  }
  return null;
}

function _execOrThrow(cmd, args, options) {
  var output = agentdesk.exec(cmd, args, options);
  if (typeof output === "string" && output.indexOf("ERROR:") === 0) {
    throw new Error(output.substring("ERROR:".length).trim() || (cmd + " failed"));
  }
  return output || "";
}

function _splitNonEmptyLines(text) {
  if (!text) return [];
  return String(text)
    .split(/\r?\n/)
    .map(function(line) { return line.trim(); })
    .filter(function(line) { return line !== ""; });
}

function _normalizeDispatchTimestamp(createdAt) {
  if (!createdAt || typeof createdAt !== "string") return null;
  if (createdAt.indexOf("T") !== -1) return createdAt;
  return createdAt.replace(" ", "T") + "Z";
}

function _resolveCompletedWorktreePath(dispatchContext, workResult) {
  return _firstPresent(
    workResult && workResult.completed_worktree_path,
    workResult && workResult.worktree_path,
    dispatchContext && dispatchContext.completed_worktree_path,
    dispatchContext && dispatchContext.worktree_path
  );
}

function _resolveCompletedBranch(worktreePath, dispatchContext, workResult) {
  var branch = _firstPresent(
    workResult && workResult.completed_branch,
    workResult && workResult.worktree_branch,
    dispatchContext && dispatchContext.completed_branch,
    dispatchContext && dispatchContext.worktree_branch,
    dispatchContext && dispatchContext.branch
  );
  if (branch) return branch;
  if (!worktreePath) return null;
  var resolved = _execOrThrow("git", ["-C", worktreePath, "branch", "--show-current"]);
  return resolved ? resolved.trim() : null;
}

function _dispatchTouchedSrcSinceCreated(worktreePath, createdAt) {
  if (!worktreePath) return false;

  var dirtySrc = _splitNonEmptyLines(
    _execOrThrow("git", ["-C", worktreePath, "status", "--porcelain", "--", "src"])
  );
  if (dirtySrc.length > 0) return true;

  var since = _normalizeDispatchTimestamp(createdAt);
  if (!since) return true;

  var baseCommit = _execOrThrow(
    "git",
    ["-C", worktreePath, "rev-list", "-n", "1", "--before=" + since, "HEAD"]
  ).trim();

  var diffArgs = baseCommit
    ? ["-C", worktreePath, "diff", "--name-only", baseCommit + "..HEAD", "--", "src"]
    : ["-C", worktreePath, "diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD", "--", "src"];

  return _splitNonEmptyLines(_execOrThrow("git", diffArgs)).length > 0;
}

function _inventoryDocsChanged(worktreePath) {
  var args = ["-C", worktreePath, "status", "--porcelain", "--"].concat(INVENTORY_DOC_PATHS);
  return _splitNonEmptyLines(_execOrThrow("git", args)).length > 0;
}

function _autoRefreshInventoryDocs(card, dispatch, dispatchContext, workResult) {
  if (dispatch.dispatch_type !== "implementation" && dispatch.dispatch_type !== "rework") return;

  var worktreePath = _resolveCompletedWorktreePath(dispatchContext, workResult);
  if (!worktreePath) {
    agentdesk.log.info("[inventory] dispatch " + dispatch.id + " skipped: no completed worktree path");
    return;
  }

  try {
    if (!_dispatchTouchedSrcSinceCreated(worktreePath, dispatch.created_at)) {
      agentdesk.log.info("[inventory] dispatch " + dispatch.id + " skipped: no src changes since dispatch start");
      return;
    }
  } catch (e) {
    var probeError = e && e.message ? e.message : String(e);
    agentdesk.log.warn(
      "[inventory] dispatch " + dispatch.id + " skipped: src-change probe failed for " +
      worktreePath + ": " + probeError
    );
    return;
  }

  try {
    agentdesk.runtime.refreshInventoryDocs(worktreePath, { timeout_ms: 60000 });
    if (!_inventoryDocsChanged(worktreePath)) {
      agentdesk.log.info("[inventory] dispatch " + dispatch.id + " refreshed generator but docs were already up to date");
      return;
    }

    // #2051 Finding 3 (P1): policy hooks run on a single actor thread
    // (`PolicyEngineActor`); long synchronous `git push` calls block every
    // other hook (OnSessionStatusChange / OnDispatchCompleted /
    // OnCardTransition) until the timeout expires. Cap each git call to a
    // tight bound so a misbehaving remote cannot stall the queue for minutes.
    // Long-term TODO: move inventory refresh to an async/queued job
    // (`agentdesk.runtime.scheduleInventoryRefresh`) and only mark metadata
    // from the hook itself.
    _execOrThrow(
      "git",
      ["-C", worktreePath, "add", "--"].concat(INVENTORY_DOC_PATHS),
      { timeout_ms: 10000 }
    );
    if (!_inventoryDocsChanged(worktreePath)) {
      agentdesk.log.info("[inventory] dispatch " + dispatch.id + " had no staged generated-doc diff");
      return;
    }

    _execOrThrow(
      "git",
      ["-C", worktreePath, "commit", "-m", "chore: refresh inventory"],
      { timeout_ms: 10000 }
    );

    var branch = _resolveCompletedBranch(worktreePath, dispatchContext, workResult);
    if (!branch) {
      throw new Error("could not resolve worktree branch for inventory push");
    }

    _execOrThrow(
      "git",
      ["-C", worktreePath, "push", "-u", "origin", branch],
      { timeout_ms: 20000 }
    );
    agentdesk.log.info("[inventory] dispatch " + dispatch.id + " auto-refreshed generated docs on " + branch);
  } catch (e) {
    var errorText = e && e.message ? e.message : String(e);
    agentdesk.log.warn(
      "[inventory] dispatch " + dispatch.id + " auto-refresh failed for " + card.id + ": " + errorText
    );
    notifyCardOwner(
      card.id,
      "module-inventory 자동 갱신 실패\n" + errorText,
      "inventory"
    );
  }
}

module.exports = {
  INVENTORY_DOC_PATHS: INVENTORY_DOC_PATHS,
  _extractRepoFromUrl: _extractRepoFromUrl,
  _firstPresent: _firstPresent,
  _execOrThrow: _execOrThrow,
  _splitNonEmptyLines: _splitNonEmptyLines,
  _normalizeDispatchTimestamp: _normalizeDispatchTimestamp,
  _resolveCompletedWorktreePath: _resolveCompletedWorktreePath,
  _resolveCompletedBranch: _resolveCompletedBranch,
  _dispatchTouchedSrcSinceCreated: _dispatchTouchedSrcSinceCreated,
  _inventoryDocsChanged: _inventoryDocsChanged,
  _autoRefreshInventoryDocs: _autoRefreshInventoryDocs
};

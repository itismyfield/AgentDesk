/** @module policies/lib/merge-conflict-resolver
 *
 * #1078: Extracted from merge-automation.js as part of the policy modularization pass.
 *
 * Low-level git/worktree primitives used by the direct-merge fast-forward
 * and cherry-pick paths plus the conflict recovery loops. Functions here
 * intentionally rely on the global `agentdesk.exec` surface — the test
 * harness injects mocks through the same global.
 */

var _mergeTextUtils = require("./merge-text-utils");
var _isCherryPickConflict = _mergeTextUtils.isCherryPickConflict;
var _isPushRejected = _mergeTextUtils.isPushRejected;

function execGitOrThrow(args) {
  var output = agentdesk.exec("git", args);
  if (typeof output === "string" && output.indexOf("ERROR") === 0) {
    throw new Error(output.replace(/^ERROR:\s*/, ""));
  }
  return typeof output === "string" ? output : "";
}

function execGitMaybe(args) {
  var output = agentdesk.exec("git", args);
  if (typeof output === "string" && output.indexOf("ERROR") === 0) {
    return null;
  }
  return typeof output === "string" ? output : "";
}

function parseWorktreeList(text) {
  var entries = [];
  var current = { path: "", branch: null };
  var lines = String(text || "").split(/\r?\n/);
  for (var i = 0; i < lines.length; i++) {
    var line = lines[i];
    if (line.indexOf("worktree ") === 0) {
      if (current.path) entries.push(current);
      current = { path: line.substring("worktree ".length), branch: null };
    } else if (line.indexOf("branch ") === 0) {
      var branch = line.substring("branch ".length);
      current.branch = branch.indexOf("refs/heads/") === 0
        ? branch.substring("refs/heads/".length)
        : branch;
    } else if (!line.trim() && current.path) {
      entries.push(current);
      current = { path: "", branch: null };
    }
  }
  if (current.path) entries.push(current);
  return entries;
}

function maybeRestoreMergeStash(mainWorktreePath, stashCreated) {
  if (!stashCreated) return null;
  var output = agentdesk.exec("git", ["-C", mainWorktreePath, "stash", "pop"]);
  if (typeof output === "string" && output.indexOf("ERROR") === 0) {
    var err = output.replace(/^ERROR:\s*/, "").trim();
    return err
      ? "stash created but restore reported conflicts: " + err
      : "stash created but restore needs manual check";
  }
  return "stash restored";
}

function maybeResetDirectMergeHead(mainWorktreePath, originalHead) {
  if (!originalHead) return null;
  var output = agentdesk.exec("git", ["-C", mainWorktreePath, "reset", "--hard", originalHead]);
  if (typeof output === "string" && output.indexOf("ERROR") === 0) {
    var err = output.replace(/^ERROR:\s*/, "").trim();
    return err
      ? "reset to original main HEAD failed: " + err
      : "reset to original main HEAD failed";
  }
  return "main worktree reset to pre-merge HEAD";
}

function retryDirectMergePush(mainWorktreePath, mainBranch) {
  var maxRetries = 3;
  var attempts = 0;
  var lastError = null;

  while (attempts <= maxRetries) {
    var pushOutput = agentdesk.exec("git", ["-C", mainWorktreePath, "push", "origin", mainBranch]);
    if (!(typeof pushOutput === "string" && pushOutput.indexOf("ERROR") === 0)) {
      return {
        ok: true,
        attempts: attempts + 1
      };
    }

    lastError = pushOutput.replace(/^ERROR:\s*/, "");
    if (!_isPushRejected(lastError) || attempts === maxRetries) {
      return {
        ok: false,
        conflict: false,
        error: lastError,
        attempts: attempts + 1
      };
    }

    attempts += 1;

    var fetchOutput = agentdesk.exec("git", ["-C", mainWorktreePath, "fetch", "origin", mainBranch]);
    if (typeof fetchOutput === "string" && fetchOutput.indexOf("ERROR") === 0) {
      return {
        ok: false,
        conflict: false,
        error: fetchOutput.replace(/^ERROR:\s*/, ""),
        attempts: attempts
      };
    }

    var rebaseOutput = agentdesk.exec("git", ["-C", mainWorktreePath, "rebase", "origin/" + mainBranch]);
    if (typeof rebaseOutput === "string" && rebaseOutput.indexOf("ERROR") === 0) {
      return {
        ok: false,
        conflict: _isCherryPickConflict(rebaseOutput),
        error: rebaseOutput.replace(/^ERROR:\s*/, ""),
        attempts: attempts,
        rebase_failed: true
      };
    }
  }

  return {
    ok: false,
    conflict: false,
    error: lastError || "direct merge push failed",
    attempts: attempts
  };
}

function tryFastForwardMain(mainWorktreePath, mainBranch, branch) {
  var baseCheck = agentdesk.exec("git", [
    "-C",
    mainWorktreePath,
    "merge-base",
    "--is-ancestor",
    mainBranch,
    branch
  ]);
  if (typeof baseCheck === "string" && baseCheck.indexOf("ERROR") === 0) {
    return false;
  }

  execGitOrThrow(["-C", mainWorktreePath, "merge", "--ff-only", branch]);
  return true;
}

function resolveCanonicalRepoRoot(worktreePath) {
  var commonDir = execGitOrThrow([
    "-C",
    worktreePath,
    "rev-parse",
    "--path-format=absolute",
    "--git-common-dir"
  ]).trim();
  return commonDir.replace(/\/\.git\/?$/, "");
}

function resolveMainWorktree(repoDir) {
  var worktreeOutput = execGitOrThrow(["-C", repoDir, "worktree", "list", "--porcelain"]);
  var worktrees = parseWorktreeList(worktreeOutput);
  if (!worktrees.length) {
    throw new Error("could not locate main worktree");
  }
  for (var i = 0; i < worktrees.length; i++) {
    if (worktrees[i].branch === "main" || worktrees[i].branch === "master") {
      return worktrees[i];
    }
  }
  return worktrees[0];
}

module.exports = {
  execGitOrThrow: execGitOrThrow,
  execGitMaybe: execGitMaybe,
  parseWorktreeList: parseWorktreeList,
  maybeRestoreMergeStash: maybeRestoreMergeStash,
  maybeResetDirectMergeHead: maybeResetDirectMergeHead,
  retryDirectMergePush: retryDirectMergePush,
  tryFastForwardMain: tryFastForwardMain,
  resolveCanonicalRepoRoot: resolveCanonicalRepoRoot,
  resolveMainWorktree: resolveMainWorktree
};

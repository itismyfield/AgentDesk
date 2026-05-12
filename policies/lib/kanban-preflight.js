/** @module policies/lib/kanban-preflight
 *
 * #1078: Extracted from kanban-rules.js as part of the policy modularization pass.
 *
 * Owns the #256 preflight checks run when a card enters the kickoff/requested
 * state and the OnCardTransition hook gates dispatch creation:
 *   - _runPreflight(cardId) — returns { status, summary } where status is one of
 *       "invalid" | "already_applied" | "consult_required" | "assumption_ok" | "clear"
 *
 * Depends on:
 *   - global `agentdesk.cards`, `agentdesk.db`, `agentdesk.exec`, `agentdesk.log`
 *   - `_extractRepoFromUrl` from `./kanban-inventory-refresh` (shared helper)
 */

var _inventory = require("./kanban-inventory-refresh");
var _extractRepoFromUrl = _inventory._extractRepoFromUrl;

function _runPreflight(cardId) {
  var c = agentdesk.cards.get(cardId);
  if (!c) return { status: "invalid", summary: "Card not found" };

  // Check 1: GitHub issue closed? (uses gh CLI since no bridge exists)
  // #2051 Finding 11 (P1): _runPreflight runs inside OnCardTransition on the
  // single policy actor thread. A bare `agentdesk.exec("gh", ...)` without a
  // timeout could block every other hook indefinitely if GitHub hangs or
  // `gh` enters an auth retry loop. Cap the call so the actor cannot stall.
  if (c.github_issue_number && c.github_issue_url) {
    var repo = _extractRepoFromUrl(c.github_issue_url);
    if (repo) {
      try {
        var ghOutput = agentdesk.exec(
          "gh",
          [
            "issue", "view", String(c.github_issue_number),
            "--repo", repo, "--json", "state", "--jq", ".state"
          ],
          { timeout_ms: 10000 }
        );
        if (ghOutput && ghOutput.trim() === "CLOSED") {
          return { status: "already_applied", summary: "GitHub issue #" + c.github_issue_number + " is closed" };
        }
      } catch (e) {
        agentdesk.log.warn("[preflight] gh issue view failed for card " + cardId + " issue #" + c.github_issue_number + ": " + e);
      }
    }
  }

  // Check 2: Already has terminal dispatch?
  var terminalDispatch = agentdesk.db.query(
    "SELECT id FROM task_dispatches WHERE kanban_card_id = ? AND dispatch_type = 'implementation' AND status = 'completed'",
    [cardId]
  );
  if (terminalDispatch.length > 0) {
    return { status: "already_applied", summary: "Implementation dispatch already completed" };
  }

  // Check 3: Description/body too short or empty?
  // #2051 Finding 22 (P3): completely empty descriptions are a different
  // signal than "short". A card with no description at all is typically the
  // shape produced by external automation (cron seeders, GH webhook hand-offs
  // that haven't pulled the issue body yet, etc.); routing it through a
  // consultation dispatch wastes tokens and time. Treat truly empty bodies as
  // `assumption_ok` with a diagnostic marker so operators can see the gap
  // without blocking dispatch. Short-but-present descriptions still get the
  // consultation path because a human wrote something — there's something to
  // clarify against.
  var body = c.description || "";
  var bodyTrimmed = body.trim();
  if (bodyTrimmed.length === 0) {
    return {
      status: "assumption_ok",
      summary: "No description provided — proceeding with issue title only (no_dod_provided)"
    };
  }
  if (bodyTrimmed.length < 30) {
    return { status: "consult_required", summary: "Issue body is too short or empty — needs clarification" };
  }

  // Check 4: No DoD section?
  if (body.indexOf("DoD") === -1 && body.indexOf("Definition of Done") === -1 && body.indexOf("완료 기준") === -1) {
    return { status: "assumption_ok", summary: "No explicit DoD found, assuming spec is sufficient" };
  }

  // All checks passed
  return { status: "clear", summary: "Preflight checks passed" };
}

module.exports = {
  _runPreflight: _runPreflight
};

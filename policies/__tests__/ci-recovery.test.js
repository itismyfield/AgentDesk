const test = require("node:test");
const assert = require("node:assert/strict");

const { loadPolicy } = require("./support/harness");

function loadCiRecoveryScenario({ cardLifecycle, runConclusion = "failure", failedJob = "Script checks", log = "audit_maintainability: hard-gate direct_discord_sends failed" }) {
  const upserts = [];
  const blockedReasons = [];
  const execCalls = [];

  const { policy, state } = loadPolicy("policies/ci-recovery.js", {
    exec(cmd, args) {
      execCalls.push({ cmd, args });
      assert.equal(cmd, "gh");
      const joined = args.join(" ");
      if (joined.includes("pr view 1878") && joined.includes("--json headRefOid")) {
        return "sha-1878";
      }
      if (joined.includes("run list")) {
        return JSON.stringify([
          {
            databaseId: 187801,
            status: "completed",
            conclusion: runConclusion,
            headSha: "sha-1878",
            event: "pull_request"
          }
        ]);
      }
      if (joined.includes("run view 187801") && joined.includes("--json jobs")) {
        return JSON.stringify({ jobs: [{ name: failedJob, conclusion: "failure" }] });
      }
      if (joined.includes("run view 187801") && joined.includes("--log-failed")) {
        return log;
      }
      if (joined.includes("run view 187801")) {
        return "X " + failedJob + " failed for fallback PR";
      }
      if (joined.includes("run rerun")) {
        return "";
      }
      throw new Error(`Unhandled exec call: ${joined}`);
    },
    extraAgentdesk: {
      ciRecovery: {
        listWaitingForCi() {
          return [{ id: "card-1878", blocked_reason: "ci:waiting" }];
        },
        getCardLifecycle(cardId) {
          assert.equal(cardId, "card-1878");
          return cardLifecycle;
        },
        getCardStatus(cardId) {
          assert.equal(cardId, "card-1878");
          return { status: cardLifecycle.status };
        },
        setBlockedReason(cardId, reason) {
          blockedReasons.push({ cardId, reason });
          return { ok: true };
        },
        getReworkCardInfo(cardId) {
          assert.equal(cardId, "card-1878");
          return {
            assigned_agent_id: "project-agentdesk",
            github_issue_number: 1760,
            title: "Fallback PR issue"
          };
        }
      },
      prTracking: {
        importLegacyOnce() {},
        load(cardId) {
          assert.equal(cardId, "card-1878");
          return {
            card_id: "card-1878",
            repo_id: "test/repo",
            worktree_path: "/tmp/card-1878",
            branch: "wt/card-1878",
            pr_number: 1878,
            head_sha: "sha-1878",
            state: "wait-ci"
          };
        },
        resolvePrInfoForCard(cardId) {
          assert.equal(cardId, "card-1878");
          return {
            number: 1878,
            repo: "test/repo",
            branch: "wt/card-1878",
            sha: "sha-1878",
            worktree_path: "/tmp/card-1878"
          };
        },
        upsert(cardId, repoId, worktreePath, branch, prNumber, headSha, trackingState, lastError) {
          upserts.push({
            cardId,
            repoId,
            worktreePath,
            branch,
            prNumber,
            headSha,
            trackingState,
            lastError
          });
          return {};
        }
      }
    }
  });

  return { policy, state, upserts, blockedReasons, execCalls };
}

test("ci-recovery marks failed checks on done fallback PRs as pending handoff", () => {
  const upserts = [];
  const blockedReasons = [];
  const execCalls = [];

  const { policy, state } = loadPolicy("policies/ci-recovery.js", {
    exec(cmd, args) {
      execCalls.push({ cmd, args });
      assert.equal(cmd, "gh");
      const joined = args.join(" ");
      if (joined.includes("pr view 1878") && joined.includes("--json headRefOid")) {
        return "sha-1878";
      }
      if (joined.includes("run list")) {
        return JSON.stringify([
          {
            databaseId: 187801,
            status: "completed",
            conclusion: "failure",
            headSha: "sha-1878",
            event: "pull_request"
          }
        ]);
      }
      if (joined.includes("run view 187801") && joined.includes("--json jobs")) {
        return JSON.stringify({ jobs: [{ name: "Script checks", conclusion: "failure" }] });
      }
      if (joined.includes("run view 187801") && joined.includes("--log-failed")) {
        return "audit_maintainability: hard-gate direct_discord_sends failed";
      }
      if (joined.includes("run view 187801")) {
        return "X Script checks failed for fallback PR";
      }
      throw new Error(`Unhandled exec call: ${joined}`);
    },
    extraAgentdesk: {
      ciRecovery: {
        listWaitingForCi() {
          return [{ id: "card-1878", blocked_reason: "ci:waiting" }];
        },
        getCardLifecycle(cardId) {
          assert.equal(cardId, "card-1878");
          return {
            status: "done",
            completed_at: "2026-05-06T12:00:00Z",
            github_issue_number: 1760,
            github_issue_url: "https://github.com/test/repo/issues/1760"
          };
        },
        getCardStatus(cardId) {
          assert.equal(cardId, "card-1878");
          return { status: "done" };
        },
        setBlockedReason(cardId, reason) {
          blockedReasons.push({ cardId, reason });
          return { ok: true };
        },
        getReworkCardInfo() {
          throw new Error("done fallback PRs must not dispatch rework");
        }
      },
      prTracking: {
        importLegacyOnce() {},
        load(cardId) {
          assert.equal(cardId, "card-1878");
          return {
            card_id: "card-1878",
            repo_id: "test/repo",
            worktree_path: "/tmp/card-1878",
            branch: "wt/card-1878",
            pr_number: 1878,
            head_sha: "sha-1878",
            state: "wait-ci"
          };
        },
        resolvePrInfoForCard(cardId) {
          assert.equal(cardId, "card-1878");
          return {
            number: 1878,
            repo: "test/repo",
            branch: "wt/card-1878",
            sha: "sha-1878",
            worktree_path: "/tmp/card-1878"
          };
        },
        upsert(cardId, repoId, worktreePath, branch, prNumber, headSha, trackingState, lastError) {
          upserts.push({
            cardId,
            repoId,
            worktreePath,
            branch,
            prNumber,
            headSha,
            trackingState,
            lastError
          });
          return {};
        }
      }
    }
  });

  policy.onTick1min();

  assert.equal(upserts.at(-1).trackingState, "pending-handoff");
  assert.equal(upserts.at(-1).prNumber, 1878);
  assert.match(upserts.at(-1).lastError, /PR #1878 has failed checks/);
  assert.match(upserts.at(-1).lastError, /https:\/\/github\.com\/test\/repo\/pull\/1878/);
  assert.match(upserts.at(-1).lastError, /Suggested next action/);
  assert.deepEqual(blockedReasons, [
    {
      cardId: "card-1878",
      reason:
        "ci:pending-handoff:PR#1878 failed checks; inspect, fix/rerun, merge, close superseded, or accept pending handoff"
    }
  ]);
  assert.equal(state.dispatchCreates.length, 0);
  assert.equal(state.statusCalls.length, 0);
  assert.ok(execCalls.some((call) => call.args.includes("run") && call.args.includes("list")));
});

test("ci-recovery leaves non-terminal fallback PR failures on the existing rework path", () => {
  const { policy, state, upserts } = loadCiRecoveryScenario({
    cardLifecycle: {
      status: "in_progress",
      completed_at: null,
      github_issue_number: 1760,
      github_issue_url: "https://github.com/test/repo/issues/1760"
    }
  });

  policy.onTick1min();

  assert.equal(state.dispatchCreates.length, 1);
  assert.equal(state.dispatchCreates[0].dispatchType, "rework");
  assert.equal(upserts.at(-1).trackingState, "wait-ci");
  assert.match(upserts.at(-1).lastError, /CI code failure/);
});

test("ci-recovery marks pipeline-terminal fallback PRs without completed_at as pending handoff", () => {
  const { policy, state, upserts, blockedReasons } = loadCiRecoveryScenario({
    cardLifecycle: {
      status: "done",
      completed_at: null,
      github_issue_number: 1760,
      github_issue_url: "https://github.com/test/repo/issues/1760"
    }
  });

  policy.onTick1min();

  assert.equal(upserts.at(-1).trackingState, "pending-handoff");
  assert.equal(state.dispatchCreates.length, 0);
  assert.match(blockedReasons.at(-1).reason, /^ci:pending-handoff:/);
});

test("ci-recovery marks transient failures on done fallback PRs as pending handoff", () => {
  const { policy, state, upserts, execCalls } = loadCiRecoveryScenario({
    cardLifecycle: {
      status: "done",
      completed_at: "2026-05-06T12:00:00Z",
      github_issue_number: 1760,
      github_issue_url: "https://github.com/test/repo/issues/1760"
    },
    runConclusion: "cancelled"
  });

  policy.onTick1min();

  assert.equal(upserts.at(-1).trackingState, "pending-handoff");
  assert.equal(state.dispatchCreates.length, 0);
  assert.ok(!execCalls.some((call) => call.args.includes("rerun")));
});

const test = require("node:test");
const assert = require("node:assert/strict");

const { createSqlRouter, loadPolicy } = require("./support/harness");

test("merge automation blocks tracked PR when required phase evidence is missing for head SHA", () => {
  const { module } = loadPolicy("policies/merge-automation.js", {
    exec(cmd, args) {
      assert.equal(cmd, "gh");
      assert.equal(JSON.stringify(args.slice(0, 3)), JSON.stringify(["run", "list", "--branch"]));
      return JSON.stringify([{
        databaseId: 101,
        status: "completed",
        conclusion: "success",
        headSha: "abc123",
        event: "push"
      }]);
    },
    dbQuery: createSqlRouter([
      {
        match: "SELECT required_phases FROM issue_specs WHERE card_id = ?",
        result: [{ required_phases: JSON.stringify(["unreal-smoke", "api-regression"]) }]
      },
      {
        match: "SELECT id FROM test_phase_runs WHERE phase_key = ?",
        result(_sql, params) {
          return params[0] === "unreal-smoke" ? [{ id: "tpr-1" }] : [];
        }
      }
    ])
  });

  const readiness = module.__test.verifyTrackedPrMergeReadiness({
    card_id: "card-1",
    repo_id: "itismyfield/AgentDesk",
    branch: "work/card-1",
    head_sha: "abc123"
  }, "abc123");

  assert.equal(readiness.ok, false);
  assert.match(readiness.reason, /missing required phase evidence/);
  assert.match(readiness.reason, /api-regression/);
});

test("merge automation accepts tracked PR when CI and all required phase evidence pass", () => {
  const { module } = loadPolicy("policies/merge-automation.js", {
    exec() {
      return JSON.stringify([{
        databaseId: 102,
        status: "completed",
        conclusion: "success",
        headSha: "def456",
        event: "push"
      }]);
    },
    dbQuery: createSqlRouter([
      {
        match: "SELECT required_phases FROM issue_specs WHERE card_id = ?",
        result: [{ required_phases: ["unreal-smoke"] }]
      },
      {
        match: "SELECT id FROM test_phase_runs WHERE phase_key = ?",
        result: [{ id: "tpr-2" }]
      }
    ])
  });

  const readiness = module.__test.verifyTrackedPrMergeReadiness({
    card_id: "card-2",
    repo_id: "itismyfield/AgentDesk",
    branch: "work/card-2",
    head_sha: "def456"
  }, "def456");

  assert.equal(readiness.ok, true);
  assert.equal(readiness.run.databaseId, 102);
});

// #1946 (a): direct-first 머지 성공 후 GH 이슈 자동 close 가 OPEN 상태 → comment
// → close 순서로 호출되는지, 이미 CLOSED 면 skip 되는지, github_issue_number 가
// 비어있으면 skip 되는지 검증.

test("direct-merge close-issue posts comment and closes when issue is OPEN", () => {
  const calls = [];
  const { module } = loadPolicy("policies/merge-automation.js", {
    exec(cmd, args) {
      calls.push([cmd, args.slice()]);
      assert.equal(cmd, "gh");
      // 첫 호출: state 조회 (issue view)
      if (args[0] === "issue" && args[1] === "view") return "OPEN\n";
      // 두번째: comment
      if (args[0] === "issue" && args[1] === "comment") return "ok";
      // 세번째: close
      if (args[0] === "issue" && args[1] === "close") return "ok";
      throw new Error("unexpected gh args: " + JSON.stringify(args));
    }
  });

  module.__test.closeGithubIssueAfterDirectMerge(
    {
      card: { id: "card-1812", github_issue_number: 1812 },
      repo_id: "itismyfield/AgentDesk",
      branch: "adk/auto/issue-1812-foo",
      head_sha: "76459e10feedfaceabcd"
    },
    { ok: true, main_branch: "main" }
  );

  // 호출 순서: state view → comment → close
  assert.equal(calls.length, 3);
  assert.equal(calls[0][1][0], "issue");
  assert.equal(calls[0][1][1], "view");
  assert.equal(calls[0][1][2], "1812");
  assert.equal(calls[1][1][0], "issue");
  assert.equal(calls[1][1][1], "comment");
  assert.equal(calls[1][1][2], "1812");
  assert.equal(calls[2][1][0], "issue");
  assert.equal(calls[2][1][1], "close");
  assert.equal(calls[2][1][2], "1812");
  // close 에 --reason completed 가 붙어야 함
  assert.ok(calls[2][1].includes("--reason"));
  assert.ok(calls[2][1].includes("completed"));
  // comment 본문에 retro #1946 + branch + short SHA 포함
  const commentBody = calls[1][1][calls[1][1].indexOf("--body") + 1];
  assert.match(commentBody, /#1946/);
  assert.match(commentBody, /adk\/auto\/issue-1812-foo/);
  assert.match(commentBody, /76459e10feed/);
});

test("direct-merge close-issue skips when issue is already CLOSED", () => {
  const calls = [];
  const { module } = loadPolicy("policies/merge-automation.js", {
    exec(cmd, args) {
      calls.push([cmd, args.slice()]);
      if (args[0] === "issue" && args[1] === "view") return "CLOSED\n";
      throw new Error("comment/close should not be called when already closed");
    }
  });

  module.__test.closeGithubIssueAfterDirectMerge(
    {
      card: { id: "card-1812", github_issue_number: 1812 },
      repo_id: "itismyfield/AgentDesk",
      branch: "adk/auto/issue-1812-foo",
      head_sha: "76459e10feedfaceabcd"
    },
    { ok: true, main_branch: "main" }
  );

  assert.equal(calls.length, 1, "only the state view should fire");
});

test("direct-merge close-issue skips when card has no github_issue_number", () => {
  const calls = [];
  const { module } = loadPolicy("policies/merge-automation.js", {
    exec() {
      calls.push("called");
      throw new Error("gh should not be called when issue number is missing");
    }
  });

  module.__test.closeGithubIssueAfterDirectMerge(
    {
      card: { id: "card-no-issue", github_issue_number: null },
      repo_id: "itismyfield/AgentDesk",
      branch: "adk/auto/no-issue",
      head_sha: "deadbeefcafebabe1234"
    },
    { ok: true, main_branch: "main" }
  );

  assert.equal(calls.length, 0);
});

test("direct-merge close-issue still closes when comment posting fails", () => {
  const calls = [];
  const { module } = loadPolicy("policies/merge-automation.js", {
    exec(cmd, args) {
      calls.push([cmd, args.slice()]);
      if (args[0] === "issue" && args[1] === "view") return "OPEN\n";
      if (args[0] === "issue" && args[1] === "comment") return "ERROR: rate limit";
      if (args[0] === "issue" && args[1] === "close") return "ok";
      throw new Error("unexpected gh args");
    }
  });

  module.__test.closeGithubIssueAfterDirectMerge(
    {
      card: { id: "card-1812", github_issue_number: 1812 },
      repo_id: "itismyfield/AgentDesk",
      branch: "adk/auto/issue-1812-foo",
      head_sha: "76459e10feedfaceabcd"
    },
    { ok: true, main_branch: "main" }
  );

  // 3 calls: view → comment(ERROR) → close. close 가 여전히 호출되어야 함.
  assert.equal(calls.length, 3);
  assert.equal(calls[2][1][1], "close");
});

// ── #3278: tick git-exec budget ──────────────────────────────────────────
// onTick5min accumulated up to 2 git execs per inspected dispatch row (×16
// rows ×N tracked cards) and blew the 5s POLICY_TICK_HOOK_TIMEOUT. The fix:
// (1) diagnostics-only rows (pending/dispatched, and cancelled rows after the
// fallback slot is taken) skip the git fallback entirely, and (2) a tick-local
// cache dedupes fallback lookups per worktree within one tick.

function dispatchRowsRouter(excludedRows, completedRows) {
  return createSqlRouter([
    {
      match: "AND status IN ('pending', 'dispatched', 'cancelled')",
      result: excludedRows
    },
    {
      match: "AND status = 'completed'",
      result: completedRows
    }
  ]);
}

test("inspectLatestCompletedWorkTarget skips git fallback for diagnostics-only dispatch rows", () => {
  const { module, state } = loadPolicy("policies/merge-automation.js", {
    exec(cmd, args) {
      throw new Error(`no git fallback expected, got: ${cmd} ${JSON.stringify(args)}`);
    },
    dbQuery: dispatchRowsRouter(
      [
        { id: "d1", status: "pending", result: null, context: JSON.stringify({ worktree_path: "/tmp/wt-a" }) },
        { id: "d2", status: "dispatched", result: null, context: JSON.stringify({ worktree_path: "/tmp/wt-a" }) }
      ],
      [
        {
          id: "d3",
          status: "completed",
          result: JSON.stringify({
            completed_worktree_path: "/tmp/wt-a",
            completed_branch: "feat/x",
            completed_commit: "abc123"
          }),
          context: null
        }
      ]
    )
  });

  const info = module.__test.inspectLatestCompletedWorkTarget("card-3278");

  assert.equal(state.execCalls.length, 0, "pending/dispatched rows must not exec git");
  assert.equal(info.target.branch, "feat/x");
  assert.equal(info.target.head_sha, "abc123");
  assert.equal(info.inspected.length, 3);
});

test("inspectLatestCompletedWorkTarget runs git fallback only for the first cancelled fallback candidate", () => {
  const { module, state } = loadPolicy("policies/merge-automation.js", {
    exec(cmd, args) {
      assert.equal(cmd, "git");
      assert.equal(args[1], "/tmp/wt-first", "only the first cancelled row may exec git");
      if (args[2] === "branch") return "fix/cancelled-branch";
      if (args[2] === "rev-parse") return "cafebabe";
      throw new Error(`unexpected git args: ${JSON.stringify(args)}`);
    },
    dbQuery: dispatchRowsRouter(
      [
        { id: "c1", status: "cancelled", result: null, context: JSON.stringify({ worktree_path: "/tmp/wt-first" }) },
        { id: "c2", status: "cancelled", result: null, context: JSON.stringify({ worktree_path: "/tmp/wt-second" }) }
      ],
      []
    )
  });

  const info = module.__test.inspectLatestCompletedWorkTarget("card-3278");

  assert.equal(state.execCalls.length, 2, "branch + rev-parse for the first cancelled row only");
  assert.equal(info.target.worktree_path, "/tmp/wt-first");
  assert.equal(info.target.branch, "fix/cancelled-branch");
  assert.equal(info.target.head_sha, "cafebabe");
  // second cancelled row stays diagnostics-only: no git-derived enrichment
  assert.equal(info.inspected[1].target.branch, null);
  assert.equal(info.inspected[1].target.head_sha, null);
});

test("tick-local cache dedupes git fallback per worktree within one tick", () => {
  const completedRow = {
    id: "d-cache",
    status: "completed",
    result: JSON.stringify({ completed_worktree_path: "/tmp/wt-cache" }),
    context: null
  };
  const { module, state } = loadPolicy("policies/merge-automation.js", {
    exec(cmd, args) {
      if (args[2] === "branch") return "feat/cached";
      if (args[2] === "rev-parse") return "deadbeef";
      throw new Error(`unexpected git args: ${JSON.stringify(args)}`);
    },
    dbQuery: dispatchRowsRouter([], [completedRow])
  });

  // simulated single tick: two cards resolving against the same worktree
  const targets = module.__test.withGitFallbackCache(() => [
    module.__test.inspectLatestCompletedWorkTarget("card-a").target,
    module.__test.inspectLatestCompletedWorkTarget("card-b").target
  ]);

  assert.equal(state.execCalls.length, 2, "second inspection must hit the cache");
  assert.equal(targets[0].branch, "feat/cached");
  assert.equal(targets[1].branch, "feat/cached");
  assert.equal(targets[1].head_sha, "deadbeef");

  // outside the wrapper the cache is disarmed — behavior unchanged
  module.__test.inspectLatestCompletedWorkTarget("card-c");
  assert.equal(state.execCalls.length, 4);
});

// ── #3278: deadline ERROR 강등 ───────────────────────────────────────────
// 5s 타임아웃 이후에도 tick actor 는 백그라운드 큐에서 훅을 계속 실행하므로
// bridge deadline 계열 에러는 "이번 tick 예산 소진 + 다음 tick 재시도"이지
// 훅 실패가 아니다. WARN 으로 강등하고 hook 은 정상 반환해야 한다.

test("onTick5min downgrades bridge deadline errors to WARN and stops the pass", () => {
  const { policy, state } = loadPolicy("policies/merge-automation.js", {
    config: { merge_automation_enabled: "true" },
    extraAgentdesk: {
      prTracking: {
        list() {
          throw new Error("bridge deadline exceeded during async bridge operation");
        }
      }
    }
  });

  assert.doesNotThrow(() => policy.onTick5min());
  assert.equal(state.logs.error.length, 0);
  assert.equal(state.logs.warn.length, 1);
  assert.match(state.logs.warn[0], /onTick5min hit bridge deadline at step processCodexReviewSignals/);
  assert.equal(state.queries.length, 0, "later steps must be deferred to the next tick");
});

test("onTick5min downgrades pre-start deadline errors thrown mid-pass", () => {
  const { policy, state } = loadPolicy("policies/merge-automation.js", {
    config: { merge_automation_enabled: "true" },
    dbQuery() {
      // first db.query happens in processManualMergeRequests (step 2)
      throw new Error("bridge deadline passed before async bridge started");
    },
    extraAgentdesk: {
      prTracking: {
        list() {
          return [];
        }
      }
    }
  });

  assert.doesNotThrow(() => policy.onTick5min());
  assert.equal(state.logs.warn.length, 1);
  assert.match(state.logs.warn[0], /at step processManualMergeRequests/);
});

test("onTick5min rethrows non-deadline errors", () => {
  const { policy } = loadPolicy("policies/merge-automation.js", {
    config: { merge_automation_enabled: "true" },
    extraAgentdesk: {
      prTracking: {
        list() {
          throw new Error("boom: schema mismatch");
        }
      }
    }
  });

  assert.throws(() => policy.onTick5min(), /schema mismatch/);
});

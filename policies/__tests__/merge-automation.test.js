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

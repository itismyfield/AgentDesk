const test = require("node:test");
const assert = require("node:assert/strict");
const { createExecRouter, loadPolicy, toPlain } = require("./support/harness");

const worktree = "/tmp/inventory-worktree";
const branch = "fix/inventory-refresh";
const cases = [
  { name: "refspec", supplied: "HEAD:refs/heads/main", current: branch, error: /invalid inventory push branch/ },
  { name: "option", supplied: "-f", current: branch, error: /invalid inventory push branch/ },
  { name: "default branch", supplied: "main", current: "main", error: /default branch/ },
  { name: "different branch", supplied: "fix/other", current: branch, error: /does not match current worktree branch/ },
  { name: "working branch", supplied: branch, current: branch },
  ...["", " ", null, undefined].flatMap((supplied) => [
    { name: `fallback working branch for ${String(supplied)}`, supplied, current: branch },
    { name: `fallback default branch for ${String(supplied)}`, supplied, current: "main", error: /default branch/ },
    { name: `unresolved branch for ${String(supplied)}`, supplied, current: "", error: /could not resolve worktree branch/ }
  ]),
  { name: "failed HEAD probe", supplied: branch, current: "ERROR: HEAD probe failed", error: /HEAD probe failed/ },
  { name: "empty HEAD probe", supplied: branch, current: "", error: /does not match current worktree branch/ }
];

test("inventory refresh only pushes the current working branch", async (t) => {
  for (const scenario of cases) {
    await t.test(scenario.name, () => {
      const { module, state } = loadPolicy("policies/lib/kanban-inventory-refresh.js", {
        exec: createExecRouter([
          {
            match: (cmd, args) => cmd === "git" && args[0] === "-C" && args[1] === worktree && args[2] === "status",
            result: " M docs/generated/route-inventory.md\n"
          },
          {
            match: (cmd, args) => cmd === "git" && args[0] === "-C" && args[1] === worktree &&
              (["branch", "rev-parse"].includes(args[2])),
            result: `${scenario.current}\n`
          },
          {
            match: (cmd, args) => cmd === "git" && args[0] === "-C" && args[1] === worktree &&
              (["add", "commit", "push"].includes(args[2])),
            result: ""
          }
        ])
      });

      module._autoRefreshInventoryDocs(
        { id: "card-inventory" },
        { id: "dispatch-inventory", dispatch_type: "implementation" },
        {},
        { completed_worktree_path: worktree, completed_branch: scenario.supplied }
      );

      assert.equal(state.execCalls.filter((call) => call.args[2] === "commit").length, 1);
      const pushes = toPlain(state.execCalls.filter((call) => call.args[2] === "push"));
      const failures = state.logs.warn.filter((message) => message.startsWith("[inventory]"));
      if (scenario.error) {
        assert.deepEqual(pushes, []);
        assert.equal(failures.length, 1);
        assert.match(failures[0], scenario.error);
      } else {
        assert.deepEqual(pushes, [{ cmd: "git", args: ["-C", worktree, "push", "-u", "origin", branch], options: { timeout_ms: 20000 } }]);
        assert.deepEqual(failures, []);
        assert.ok(state.logs.info.some((message) => message.endsWith("auto-refreshed generated docs on " + branch)));
      }
    });
  }
});

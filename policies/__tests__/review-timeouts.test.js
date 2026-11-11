const test = require("node:test");
const assert = require("node:assert/strict");
const { DatabaseSync } = require("node:sqlite");

const { loadPolicy } = require("./support/harness");

for (const dispatchType of ["review", "review-decision"]) {
  for (const status of ["pending", "dispatched", "completed", "failed"]) {
    test(`review timeouts classify second-round ${dispatchType} (${status})`, () => {
      const db = new DatabaseSync(":memory:");
      try {
        db.exec(`
          CREATE TABLE kanban_cards (
            id TEXT, title TEXT, github_issue_number INTEGER, assigned_agent_id TEXT,
            status TEXT, deleted_at TEXT, review_status TEXT, review_entered_at TEXT
          );
          CREATE TABLE task_dispatches (
            id TEXT, kanban_card_id TEXT, dispatch_type TEXT, status TEXT,
            review_round INTEGER, completed_at TEXT, updated_at TEXT
          );
          INSERT INTO kanban_cards VALUES (
            'card-review-2', 'Second review', 8088, 'agent-review',
            'review', NULL, 'reviewing', datetime('now', '-45 minutes')
          );
          INSERT INTO task_dispatches VALUES (
            'round-1', 'card-review-2', 'review', 'completed', 1,
            datetime('now', '-60 minutes'), datetime('now', '-60 minutes')
          );
        `);
        db.prepare("INSERT INTO task_dispatches VALUES (?, ?, ?, ?, 2, NULL, datetime('now', '-45 minutes'))")
          .run("round-2", "card-review-2", dispatchType, status);
        const selected = [];
        const { policy, state } = loadPolicy("policies/timeouts.js", {
          dbQuery(sql, params) {
            const rows = db.prepare(sql).all(...params);
            selected.push(rows.length);
            return rows;
          }
        });

        policy._section_C();
        policy._section_N();

        assert.equal(state.queries.length, 3);
        if (status === "pending" || status === "dispatched") {
          assert.deepEqual(selected, [0, 0, 0]);
          assert.deepEqual(state.manualInterventions, []);
        } else {
          assert.ok(selected[0] > 0);
          assert.equal(selected[1], 1);
          assert.ok(state.manualInterventions.every((call) =>
            call.cardId === "card-review-2" && call.options.review === true));
          assert.equal(state.manualInterventions.length, selected[0] + selected[1]);
        }
      } finally {
        db.close();
      }
    });
  }
}

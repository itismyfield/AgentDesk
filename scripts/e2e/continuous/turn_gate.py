"""Budget gate at the harness's send boundary: one reserved turn at a time, settled by its measured usage.

Before any input the previous turn must be settled. Unknown or over-limit usage, an exhausted budget
or an unplanned prompt stops every later input of the run.
"""

import re
import time

from . import usage

# Text commands such as `!clear` are handled by the server and start no provider turn.
COMMAND = re.compile(r"^![a-z][a-z_-]*(?:\s|$)")


class Stop(RuntimeError):
    """No further input may be sent in this run."""


class TurnGate:
    def __init__(self, guard, provider, markers, turn_tokens, transcript, wait_s):
        self.guard, self.provider, self.planned = guard, provider, list(markers)
        self.turn_tokens, self.transcript, self.wait_s = turn_tokens, transcript, wait_s
        self.open, self.spent, self.stopped = None, [], None

    def _marks(self):
        try:
            return [usage.mark(self.transcript())]
        except (OSError, ValueError, KeyError):
            return []  # A cell without a session yet starts one; its transcript is found when the turn settles.

    def _measure(self, turn):
        marks = list(turn["marks"])
        try:
            latest = self.transcript()
        except (OSError, ValueError, KeyError):
            latest = None
        if latest is not None and latest not in {item["path"] for item in marks}:
            marks.append({"path": latest, "offset": 0})
        return usage.turn(self.provider, marks, turn["marker"])

    def settle(self):
        """Close the open turn with its real usage; the provider may still be writing it, so wait briefly."""
        if self.open is None:
            return
        deadline = time.monotonic() + self.wait_s
        spent = self._measure(self.open)
        while spent is None and time.monotonic() < deadline:
            time.sleep(0.5)
            spent = self._measure(self.open)
        # Only a measured or timed-out turn is closed, so an interrupted wait is settled again later.
        self.open = None
        self.spent.append(spent)
        self.guard.finish(sum(spent.values()) if spent is not None else None)
        if self.guard.pending is not None:
            self.stopped = "TURN_USAGE_UNKNOWN" if spent is None else "TURN_OVER_BUDGET"

    def before_send(self, content):
        if self.stopped is None:
            self.settle()
        if self.stopped is None and not COMMAND.match(content):
            marker = next((item for item in self.planned if item in content), None)
            if marker is None:
                self.stopped = "UNPLANNED_PROVIDER_INPUT"
            elif not self.guard.start(self.turn_tokens):
                self.stopped = "TURN_BUDGET_EXHAUSTED"
            else:
                self.planned.remove(marker)
                self.open = {"marker": marker, "marks": self._marks()}
        if self.stopped is not None:
            raise Stop(self.stopped)

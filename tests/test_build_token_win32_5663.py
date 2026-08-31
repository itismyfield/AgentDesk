from __future__ import annotations

import importlib.util
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "scripts" / "build_token_win32.py"
SPEC = importlib.util.spec_from_file_location("build_token_win32_red", HELPER)
assert SPEC and SPEC.loader
module = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(module)


class _TraceApi:
    def assign_job(self, _job: object, _child: object, trace: list[str]) -> None:
        trace.append("assign-job")

    def resume_primary(self, _child: object, trace: list[str]) -> None:
        trace.append("resume-primary")

    def close_primary(self, _child: object, trace: list[str]) -> None:
        trace.append("close-thread")


class PortableWin32BuildTokenContractTests(unittest.TestCase):
    def test_suspended_child_is_assigned_before_primary_thread_resumes(self):
        trace = ["create-suspended"]
        module._assign_and_resume(_TraceApi(), object(), object(), trace)
        self.assertEqual(
            trace,
            ["create-suspended", "assign-job", "resume-primary", "close-thread"],
        )


if __name__ == "__main__":
    unittest.main()

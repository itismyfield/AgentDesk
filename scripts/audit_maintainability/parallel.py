"""Run audit checks in worker processes with serial-identical results.

Checks that share a whole-tree scan run in one worker under ``common.run_cache``;
every other check shares a final worker. The harness replays each check's
captured stdout/stderr and its first failure in check order, so output, exit
code, and exception choice match a serial run.
"""

from __future__ import annotations

import importlib
import io
import multiprocessing
import pickle
import traceback
from concurrent.futures import ProcessPoolExecutor
from contextlib import redirect_stderr, redirect_stdout
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

from . import common
from .checks import CheckSpec
from .common import Finding

# Checks sharing an expensive scan, heaviest first; in-group order follows check order.
SHARED_SCAN_GROUPS = (
    ("giant_files", "giant_file_ratchet", "parent_test_residue"),
    ("low_level_accessors", "duplicate_conditions"),
    ("module_layers",),
)


@dataclass(frozen=True)
class CheckOutcome:
    key: str
    findings: list[Finding] | None
    stdout: str
    stderr: str
    error: BaseException | None = None
    error_traceback: str = ""


def can_run(specs: Sequence[CheckSpec]) -> bool:
    """Workers re-import checks by module, so in-process patches would be lost."""

    default_root = Path(common.__file__).resolve().parent.parent.parent
    if (common.REPO_ROOT, common.SRC_ROOT, common.SCRIPTS_ROOT) != (
        default_root, default_root / "src", default_root / "scripts"
    ):
        return False
    for spec in specs:
        module = importlib.import_module(spec.runner.__module__)
        if getattr(module, "CHECK", None) is not spec:
            return False
    return True


def plan(specs: Sequence[CheckSpec]) -> list[list[CheckSpec]]:
    position = {spec.key: index for index, spec in enumerate(specs)}
    tasks, grouped = [], set()
    for group in SHARED_SCAN_GROUPS:
        members = sorted((key for key in group if key in position), key=position.__getitem__)
        if members:
            tasks.append([specs[position[key]] for key in members])
            grouped.update(members)
    rest = [spec for spec in specs if spec.key not in grouped]
    if rest:
        tasks.append(rest)
    return tasks


def run_task(entries: list[tuple[str, str, set[str]]]) -> list[CheckOutcome]:
    """Worker body: run checks in order, stopping at the first failure like the serial loop."""

    outcomes = []
    with common.run_cache():
        for module_name, key, allowlist in entries:
            spec = importlib.import_module(module_name).CHECK
            if spec.key != key:
                raise RuntimeError(f"{module_name} exports {spec.key}, expected {key}")
            out, err = io.StringIO(), io.StringIO()
            try:
                with redirect_stdout(out), redirect_stderr(err):
                    findings = list(spec.runner(allowlist))
            except BaseException as exc:  # replayed by the harness in check order
                outcomes.append(CheckOutcome(key, None, out.getvalue(), err.getvalue(),
                                             _picklable(exc), traceback.format_exc()))
                break
            outcomes.append(CheckOutcome(key, findings, out.getvalue(), err.getvalue()))
    return outcomes


def _picklable(exc: BaseException) -> BaseException:
    try:
        pickle.loads(pickle.dumps(exc))
    except Exception:
        return RuntimeError(f"{type(exc).__name__}: {exc}")
    return exc


def collect(
    specs: Sequence[CheckSpec], runner_allowlists: dict[str, set[str]], jobs: int
) -> dict[str, CheckOutcome]:
    tasks = plan(specs)
    context = multiprocessing.get_context("spawn")
    outcomes: dict[str, CheckOutcome] = {}
    with ProcessPoolExecutor(max_workers=min(jobs, len(tasks)), mp_context=context) as pool:
        futures = [
            pool.submit(run_task, [(spec.runner.__module__, spec.key, runner_allowlists[spec.key])
                                   for spec in task])
            for task in tasks
        ]
        for future in futures:
            outcomes.update((outcome.key, outcome) for outcome in future.result())
    return outcomes

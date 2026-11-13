"""Shared provenance contract for an immutable PR merge candidate."""

import subprocess


def pr_comparison_base(candidate: str, event_base: str, head: str,
                       parents: list[str], git) -> str:
    """Accept only the event head and a forward advance of the event base."""
    mismatch = "event/base/head/merge object provenance mismatch"
    if len(parents) != 3 or parents != [candidate, parents[1], head]:
        raise RuntimeError(mismatch)
    comparison_base = parents[1]
    if comparison_base != event_base:
        try:
            git("merge-base", "--is-ancestor", event_base, comparison_base)
        except subprocess.CalledProcessError as error:
            if error.returncode == 1:
                raise RuntimeError(mismatch) from error
            raise
    return comparison_base

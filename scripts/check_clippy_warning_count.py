#!/usr/bin/env python3
"""Count Cargo diagnostics and enforce the measured Ubuntu warning ceiling."""
import argparse
import json
import os
import re
import subprocess
from pathlib import Path

COMMAND = "cargo clippy --workspace --all-targets --all-features -- -W clippy::all"
ENVIRONMENT = ("toolchain", "host", "rustc_commit", "runner", "command")


def measure(text):
    warnings = 0
    finished = False
    for line in text.splitlines():
        event = json.loads(line)
        if not isinstance(event, dict) or event.get("reason") not in {"compiler-message", "compiler-artifact", "build-script-executed", "build-finished"} or finished:
            raise ValueError("invalid or post-finish Cargo event")
        if event["reason"] == "compiler-message":
            message = event["message"]
            level = message["level"]
            if level not in {"warning", "note", "help", "failure-note"}:
                raise ValueError("compiler error or invalid diagnostic level")
            warnings += level == "warning"
        elif event["reason"] == "build-finished":
            if event.get("success") is not True:
                raise ValueError("unsuccessful build")
            finished = True
    if not finished:
        raise ValueError("missing build-finished")
    return warnings


def toolchain():
    rustc = subprocess.check_output(["rustc", "-Vv"], text=True)
    clippy = subprocess.check_output(["cargo", "clippy", "-V"], text=True).strip()
    fields = dict(line.split(": ", 1) for line in rustc.splitlines() if ": " in line)
    commit = fields["commit-hash"]
    match = re.fullmatch(r"clippy [0-9.]+ \(([0-9a-f]+) [0-9-]+\)", clippy)
    if not re.fullmatch(r"[0-9a-f]{40}", commit) or not match or not commit.startswith(match[1]):
        raise ValueError("rustc/Clippy toolchain mismatch")
    return dict(toolchain=fields["release"], host=fields["host"], rustc_commit=commit,
                rustc_verbose=rustc, clippy_version=clippy)


def load_baseline(text):
    baseline = json.loads(text)
    if (not isinstance(baseline, dict) or baseline.get("schema") != 1
            or type(baseline.get("warnings")) is not int or baseline["warnings"] < 0
            or any(not isinstance(baseline.get(key), str) or not baseline[key] for key in ENVIRONMENT)
            or baseline["command"] != COMMAND):
        raise ValueError("invalid warning baseline")
    return baseline


def enforce(report, path, base_ref):
    baseline = load_baseline(path.read_text())
    if any(report[key] != baseline[key] for key in ENVIRONMENT):
        raise ValueError("observation environment differs from warning baseline")
    root = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
    relative = path.resolve().relative_to(root.resolve()).as_posix()
    base = subprocess.check_output(["git", "rev-parse", "--verify", "--end-of-options", base_ref + "^{commit}"], text=True).strip()
    present = subprocess.check_output(["git", "ls-tree", "--name-only", base, "--", relative], text=True).strip()
    if present:
        previous = load_baseline(subprocess.check_output(["git", "show", base + ":" + relative], text=True))
        if baseline["warnings"] > previous["warnings"]:
            raise ValueError("warning baseline increase is forbidden")
        if any(baseline[key] != previous[key] for key in ENVIRONMENT):
            raise ValueError("warning baseline environment change requires a separate rollout")
    report.update(baseline=baseline["warnings"], baseline_base=base)
    if report["warnings"] > baseline["warnings"]:
        raise ValueError(f"warning total {report['warnings']} exceeds baseline {baseline['warnings']}")
    report["gate"] = "PASS"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--base-ref")
    args = parser.parse_args()
    if args.baseline and not args.base_ref:
        parser.error("--baseline requires --base-ref for no-increase admission")
    report = dict(valid=False, sha=os.getenv("GITHUB_SHA"), runner=os.getenv("RUNNER_OS"),
                  source="ci-pr lint" if os.getenv("GITHUB_EVENT_NAME") == "pull_request" else "ci-main lint", command=COMMAND,
                  transport_command=COMMAND.replace(" -- -W", " --message-format=json -- -W"))
    try:
        report.update(toolchain())
        if args.baseline:
            baseline = load_baseline(args.baseline.read_text())
            if any(report[key] != baseline[key] for key in ENVIRONMENT):
                raise ValueError("observation environment differs from warning baseline")
        report.update(valid=True, warnings=measure(Path(args.input).read_text()))
        if args.baseline:
            enforce(report, args.baseline, args.base_ref)
    except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        report["error"] = str(error)
        if args.baseline:
            report["gate"] = "FAIL"
    Path(args.output).write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report))
    return 0 if report["valid"] and "error" not in report else 1


if __name__ == "__main__":
    raise SystemExit(main())

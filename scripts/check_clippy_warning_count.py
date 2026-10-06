#!/usr/bin/env python3
"""Observe Cargo JSON diagnostics; invalid streams never become zero debt."""
import argparse
import json
import os
from pathlib import Path

COMMAND = "cargo clippy --workspace --all-targets --all-features -- -W clippy::all"


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


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    report = dict(valid=False, sha=os.getenv("GITHUB_SHA"), runner=os.getenv("RUNNER_OS"),
                  toolchain="1.94.1", source="ci-main lint", command=COMMAND,
                  transport_command=COMMAND.replace(" -- -W", " --message-format=json -- -W"))
    try:
        report.update(valid=True, warnings=measure(Path(args.input).read_text()))
    except (OSError, ValueError, KeyError, TypeError) as error:
        report["error"] = str(error)
    Path(args.output).write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report))
    return 0 if report["valid"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

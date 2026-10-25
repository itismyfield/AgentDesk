#!/usr/bin/env python3
"""Reject a `cargo test` name filter in CI Main, CI Nightly and Trusted macOS.

A libtest filter that matches nothing still exits 0, so these workflows run whole test targets and
leave lib test selection to the nextest jobs. Only the exact commands in ALLOWED keep a filter.
A filter is a positional word on either side of `--`; a word built from a shell expansion cannot
be judged and fails too.
"""

import argparse
import re
import shlex
import sys
from pathlib import Path

import yaml

WORKFLOWS = (".github/workflows/ci-main.yml", ".github/workflows/ci-nightly.yml",
             ".github/workflows/ci-macos-trusted.yml")
ALLOWED = frozenset({"cargo test --doc ClaudeBinary"})
CARGO_VALUE = frozenset({"--test", "--bin", "--example", "--bench", "-p", "--package", "--exclude", "-F",
                         "--features", "--profile", "--target", "--target-dir", "--manifest-path", "-j",
                         "--jobs", "--message-format", "--color", "--config", "-Z"})
LIBTEST_VALUE = frozenset({"--skip", "--logfile", "--format", "--color", "-Z", "--test-threads",
                           "--shuffle-seed"})
CONTROL = frozenset({";", "&&", "||", "|", "&", "(", ")", "do", "done", "then", "fi"})
REDIRECT = re.compile(r"\d*[<>]")


def commands(script):
    """Each `cargo test ...` word list in a shell script, up to the line end or next control operator."""
    found = []
    for line in script.replace("\\\n", " ").splitlines():
        lexer = shlex.shlex(line, posix=True, punctuation_chars=";&|()")
        lexer.whitespace_split, lexer.commenters = True, ""
        words = list(lexer)
        words = words[:next((n for n, word in enumerate(words) if word.startswith("#")), len(words))]
        for index in range(len(words) - 1):
            if words[index:index + 2] == ["cargo", "test"]:
                end = next((n for n in range(index + 2, len(words))
                            if words[n] in CONTROL or REDIRECT.match(words[n])), len(words))
                found.append(words[index:end])
    return found


def filters(words):
    """Positional words of one `cargo test` command: name filters, or expansions that may be one."""
    found, values, after = [], CARGO_VALUE, False
    index = 2
    while index < len(words):
        word = words[index]
        if word == "--" and not after:
            values, after = LIBTEST_VALUE, True
        elif word in values:
            index += 1
        elif "$" in word or "`" in word or not word.startswith("-"):
            found.append(word)
        index += 1
    return found


def findings(repo_root):
    errors = []
    for relative in WORKFLOWS:
        document = yaml.safe_load((repo_root / relative).read_text("utf-8")) or {}
        for job_id, job in (document.get("jobs") or {}).items():
            for step in (job or {}).get("steps") or []:
                run = step.get("run") if isinstance(step, dict) else None
                if not isinstance(run, str):
                    continue
                try:
                    found = commands(run)
                except ValueError as error:
                    errors.append(f"{relative} {job_id} {step.get('name')!r}: unreadable run script ({error})")
                    continue
                for words in found:
                    rendered = " ".join(words)
                    if rendered not in ALLOWED and filters(words):
                        errors.append(f"{relative} {job_id} {step.get('name')!r}: `{rendered}` carries a name "
                                      f"filter {filters(words)}; run whole targets, nextest selects lib tests")
    return errors


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[2])
    args = parser.parse_args(argv)
    errors = findings(args.repo_root)
    for error in errors:
        print(f"ERROR cargo-test-filter: {error}", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())

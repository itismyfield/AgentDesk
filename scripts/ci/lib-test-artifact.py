#!/usr/bin/env python3
"""Prepare one checkout-bound Ubuntu libtest binary; fail safely to Cargo."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


BUILD = ["cargo", "test", "--locked", "--lib", "--no-run",
         "--message-format=json"]
FILES = ("lib-tests", "plain.list", "ignored.list")
ROOT = Path(__file__).resolve().parents[2]


def _capture(argv, root, env=None):
    proc = subprocess.run(argv, cwd=root, env=env, capture_output=True,
                          text=True)
    if proc.returncode:
        raise ValueError(f"{argv[0]} exited {proc.returncode}: {proc.stderr[-500:]}")
    return proc.stdout


def _build_env():
    env = dict(os.environ)
    env.setdefault("CARGO_PROFILE_DEV_DEBUG", "0")
    env.setdefault("CARGO_PROFILE_TEST_DEBUG", "0")
    if any(env[key] != "0" for key in
           ("CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG")):
        raise ValueError("shared libtest requires dev/test debug=0")
    return env


def _contract(root):
    env = _build_env()
    rust = dict(line.split(": ", 1) for line in
                _capture(["rustc", "-vV"], root).splitlines() if ": " in line)
    if sys.platform != "linux" or rust.get("host") != "x86_64-unknown-linux-gnu":
        raise ValueError("shared libtest requires the Ubuntu x86_64 host")
    if rust.get("release") != "1.94.1":
        raise ValueError("shared libtest requires Rust 1.94.1")
    if not env.get("GITHUB_RUN_ID") or not env.get("GITHUB_RUN_ATTEMPT"):
        raise ValueError("shared libtest requires current run ID and attempt")
    flags = {key: value for key, value in env.items()
             if key.startswith(("CARGO_PROFILE_", "CARGO_BUILD_", "CARGO_TARGET_"))
             or key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC",
                        "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER")}
    return {"version": 1, "sha": _capture(["git", "rev-parse", "HEAD"], root).strip(),
            "root": str(root.resolve()), "host": rust["host"],
            "rust": rust["release"], "run_id": env["GITHUB_RUN_ID"],
            "run_attempt": env["GITHUB_RUN_ATTEMPT"], "build": BUILD,
            "profile": "default-lib-test", "features": "default", "flags": flags}


def _checksum(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _inventory(text):
    ids = set()
    summary = None
    benches = 0
    for line in text.splitlines():
        if not line:
            continue
        match = re.fullmatch(r"(\S+): (test|benchmark)", line)
        if match:
            if summary is not None or match[1] in ids:
                raise ValueError("duplicate or misplaced inventory ID")
            if match[2] == "test":
                ids.add(match[1])
            else:
                benches += 1
            continue
        match = re.fullmatch(r"(\d+) tests?, (\d+) benchmarks?", line)
        if not match or summary is not None:
            raise ValueError("invalid libtest inventory format")
        summary = (int(match[1]), int(match[2]))
    if summary != (len(ids), benches):
        raise ValueError("libtest inventory count mismatch")
    return ids


def prepare(directory, root=ROOT):
    root = root.resolve()
    metadata = _contract(root)
    print("lib-test artifact: build " + " ".join(BUILD), file=sys.stderr)
    output = _capture(BUILD, root, _build_env())
    executables = set()
    for line in output.splitlines():
        item = json.loads(line)
        if (item.get("reason") == "compiler-artifact"
                and item.get("profile", {}).get("test") is True
                and "lib" in item.get("target", {}).get("kind", [])
                and item.get("executable")):
            if item["profile"].get("debuginfo") != 0:
                raise ValueError("Cargo libtest profile has nonzero debuginfo")
            executables.add(item["executable"])
    if len(executables) != 1:
        raise ValueError("Cargo must produce exactly one libtest executable")
    directory.mkdir(parents=True, exist_ok=True)
    shutil.copy2(executables.pop(), directory / "lib-tests")
    plain = _capture([str((directory / "lib-tests").resolve()), "--list"], root)
    ignored = _capture([str((directory / "lib-tests").resolve()),
                        "--list", "--ignored"], root)
    if not _inventory(plain) or not _inventory(ignored) <= _inventory(plain):
        raise ValueError("empty or inconsistent compiled inventory")
    (directory / "plain.list").write_text(plain)
    (directory / "ignored.list").write_text(ignored)
    metadata["checksums"] = {name: _checksum(directory / name) for name in FILES}
    (directory / "metadata.json").write_text(json.dumps(metadata, sort_keys=True) + "\n")
    print("lib-test artifact: prepared one lib build", file=sys.stderr)


def validate(directory, root=ROOT):
    """Validate provenance and executable inventory before sharing any input."""
    root = root.resolve()
    metadata = json.loads((directory / "metadata.json").read_text())
    expected = _contract(root)
    if {key: metadata.get(key) for key in expected} != expected:
        raise ValueError("artifact checkout/build/run contract mismatch")
    if metadata.get("checksums") != {name: _checksum(directory / name) for name in FILES}:
        raise ValueError("artifact checksum mismatch")
    binary = (directory / "lib-tests").resolve()
    if not os.access(binary, os.X_OK):
        raise ValueError("artifact binary is not executable")
    inventories = []
    for name, args in (("plain.list", ["--list"]),
                       ("ignored.list", ["--list", "--ignored"])):
        stored = (directory / name).read_text()
        ids = _inventory(stored)
        actual = _capture([str(binary), *args], root)
        if actual != stored:
            raise ValueError("live binary inventory differs from artifact")
        inventories.append(ids)
    if not inventories[0] or not inventories[1] <= inventories[0]:
        raise ValueError("empty or inconsistent compiled inventory")
    return binary


def validated_inventory(root, ignored=False):
    directory = os.environ.get("ADK_LIB_TEST_ARTIFACT")
    if not directory:
        return None
    try:
        directory = Path(directory)
        validate(directory, Path(root))
        return (directory / ("ignored.list" if ignored else "plain.list")).read_text()
    except (OSError, ValueError, TypeError, KeyError) as error:
        print(f"lib-test artifact: inventory fallback: {error}", file=sys.stderr)
        return None


def _libtest_args(command, root):
    if command[:2] != ["cargo", "test"]:
        raise ValueError("only cargo test commands may use shared libtest")
    before, marker, after = [], False, []
    for word in command[2:]:
        if word == "--" and not marker:
            marker = True
        elif marker:
            after.append(word)
        else:
            before.append(word)
    filters = []
    has_lib = False
    index = 0
    while index < len(before):
        word = before[index]
        if word == "--lib":
            has_lib = True
        elif word == "--locked":
            pass
        elif word == "--manifest-path":
            index += 1
            if index == len(before) or Path(before[index]).resolve() != root / "Cargo.toml":
                raise ValueError("foreign manifest cannot use shared libtest")
        elif word.startswith("-"):
            raise ValueError("unsupported Cargo option")
        else:
            filters.append(word)
        index += 1
    if not has_lib or len(filters) > 1:
        raise ValueError("shared libtest supports only default lib test commands")
    return filters + after


def run(command, root=ROOT):
    root = root.resolve()
    if command[:2] != ["cargo", "test"]:
        print("lib-test artifact: expected cargo test command", file=sys.stderr)
        return 1
    directory = os.environ.get("ADK_LIB_TEST_ARTIFACT")
    try:
        args = _libtest_args(command, root)
        if not directory:
            raise ValueError("artifact input absent")
        binary = validate(Path(directory), root)
    except (OSError, ValueError, TypeError, KeyError) as error:
        print(f"lib-test artifact: Cargo fallback: {error}", file=sys.stderr)
        return subprocess.call(command, cwd=root)
    print("lib-test artifact: using validated binary", file=sys.stderr)
    # An executed test failure is final; only validation failure permits Cargo.
    try:
        return subprocess.call([str(binary), *args], cwd=root)
    except OSError as error:
        print(f"lib-test artifact: execution failed: {error}", file=sys.stderr)
        return 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="operation", required=True)
    for operation in ("prepare", "validate", "list"):
        sub = subparsers.add_parser(operation)
        sub.add_argument("--directory", required=True, type=Path)
        if operation == "list":
            sub.add_argument("--ignored", action="store_true")
    subparsers.add_parser("run").add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    try:
        if args.operation == "run":
            command = args.command[1:] if args.command[:1] == ["--"] else args.command
            return run(command)
        if args.operation == "prepare":
            prepare(args.directory)
        elif args.operation == "validate":
            print(validate(args.directory))
        else:
            validate(args.directory)
            print((args.directory / ("ignored.list" if args.ignored else "plain.list")).read_text(),
                  end="")
        return 0
    except (OSError, ValueError, TypeError, KeyError) as error:
        print(f"lib-test artifact: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())

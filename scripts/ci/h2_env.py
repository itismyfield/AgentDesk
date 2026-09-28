"""Shared H2 host and environment preparation; the shell retains the Python exec boundary."""
from __future__ import annotations

import argparse
import os
import re
import shlex
import subprocess
import sys

LANES = {"linux": "x86_64-unknown-linux-gnu", "macos": "aarch64-apple-darwin"}
WRAPPERS = ("RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER",
            "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER")
CLEAR = (*WRAPPERS, "CARGO", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS", "CARGO_BUILD_TARGET",
         "RUSTC", "CARGO_BUILD_RUSTC", "RUSTC_BOOTSTRAP", "CLIPPY_ARGS")
CLEAR_RE = re.compile(r"CARGO_TARGET_.*_(?:RUSTFLAGS|RUNNER|LINKER)|CARGO_(?:PROFILE|UNSTABLE|FEATURE|CFG)_.*|__CARGO.*")


class HostMismatch(RuntimeError):
    pass


def check_host(lane: str, version: str | None = None) -> str:
    version = version if version is not None else subprocess.check_output(["rustc", "-vV"], text=True)
    host = next((line.removeprefix("host: ") for line in version.splitlines() if line.startswith("host: ")), "")
    if host != LANES[lane]:
        want = "arm64 macOS" if lane == "macos" else LANES[lane]
        raise HostMismatch(f"H2 measurement requires {want} host (got '{host}')")
    return host


def cleared_keys() -> set[str]:
    return set(CLEAR) | {key for key in os.environ if CLEAR_RE.fullmatch(key)}


def environment(mode: str = "measure") -> dict[str, str]:
    if mode not in ("measure", "map", "driver"):
        raise ValueError(f"unknown H2 environment mode: {mode}")
    cleared = cleared_keys()
    full = {key: value for key, value in os.environ.items() if key not in cleared}
    full["CARGO_INCREMENTAL"] = "0"
    full.update(dict.fromkeys(WRAPPERS, ""))
    if mode == "driver":
        full["RUSTC_BOOTSTRAP"] = "1"
    return full


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lane", choices=LANES, required=True)
    parser.add_argument("--shell", action="store_true", help="emit fixed shell environment commands; never exec")
    args = parser.parse_args(argv)
    try:
        check_host(args.lane)
        installed = subprocess.check_output(["rustup", "component", "list", "--installed"], text=True)
        if not any(line.startswith("clippy") for line in installed.splitlines()):
            raise HostMismatch("h2: clippy component is not installed for the active toolchain")
        if args.shell:
            print("unset " + " ".join(shlex.quote(key) for key in sorted(cleared_keys())))
            print("export " + " ".join(key + "=" for key in WRAPPERS))
            print("export CARGO_INCREMENTAL=" + environment()["CARGO_INCREMENTAL"])
    except HostMismatch as exc:
        print(exc, file=sys.stderr)
        return 3
    except (OSError, subprocess.CalledProcessError) as exc:
        print(f"h2: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

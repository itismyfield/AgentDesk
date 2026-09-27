#!/usr/bin/env python3
"""H2 R-O module map: build tools/modmap-driver, prove it on its canary, then map the root lib's file modules.

Only a complete map written by this run is accepted; scripts/ci/h2_depinfo.py judges it. With --inert and no
baseline the repo map is skipped, and so is everything else unless --canary asks for the driver self-test.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import h2_depinfo  # noqa: E402
import h2_measure as m  # noqa: E402

DRIVER = "tools/modmap-driver"
MIN_MODULES = 1000
# Every problem the canary must raise, in order; a driver that drops or adds one has drifted.
CANARY_PROBLEMS = [
    "R-O: inline module crate::wrapped is declared by a macro expansion",
    "R-O: macro-made module crate::wrapped wraps hand-written items from src/lib.rs",
    "R-O: macro-made module crate::named wraps hand-written items from src/lib.rs",
    "R-O: macro-made module crate::keyword wraps hand-written items from src/lib.rs",
    "R-O: include! splices src/shared.rs into crate::spliced",
    "R-O: file module crate::{fn probe}::injected (src/shared.rs) is declared by a macro expansion",
    "R-O: file module crate::{fn probe}::injected (src/shared.rs) is declared inside fn:probe",
    "R-O: file module crate::{fn probe}::injected (src/shared.rs) carries a macro-made attribute",
]

class ModmapError(RuntimeError):
    pass

def cargo(root: Path, *args: str, **env: str) -> None:
    """No rustc wrapper: sccache cannot wrap the driver, and a cache hit would skip writing the map."""
    full = {k: v for k, v in os.environ.items() if k not in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET")}
    full.pop("RUSTC_BOOTSTRAP", None)
    full.update(RUSTC_WRAPPER="", CARGO_BUILD_RUSTC_WRAPPER="", CARGO_INCREMENTAL="0", **env)
    code = subprocess.run(["cargo", *args], cwd=root, env=full).returncode
    if code:
        raise ModmapError(f"`cargo {' '.join(args)}` failed ({code})")

def build_driver(root: Path) -> Path:
    listed = subprocess.run(["rustup", "component", "list", "--installed"], cwd=root, capture_output=True, text=True)
    if not any(line.startswith("rustc-dev") for line in listed.stdout.splitlines()):
        if subprocess.run(["rustup", "component", "add", "rustc-dev"], cwd=root).returncode:
            raise ModmapError("cannot install the rustc-dev component the driver builds against")
    target = root / "target/modmap-driver"
    # rustc_private is nightly-gated; the bootstrap flag stays on this build, never on a crate the driver checks
    cargo(root, "build", "--release", "--locked", "--manifest-path", f"{DRIVER}/Cargo.toml",
          "--target-dir", str(target), RUSTC_BOOTSTRAP="1")
    return target / "release/modmap-driver"

def map_modules(root: Path, driver: Path, crate: Path, out: Path, min_modules: int, *extra: str) -> list:
    """The driver's rows for `crate`'s lib; a map this run did not write, or wrote short, is an error."""
    out.parent.mkdir(parents=True, exist_ok=True)
    out.unlink(missing_ok=True)
    marker = out.with_name(out.name + ".start")
    marker.touch()
    start = marker.stat().st_mtime_ns  # the file clock, which also stamps the map
    cargo(root, "check", "--lib", "-q", "--manifest-path", str(crate / "Cargo.toml"), *extra,
          RUSTC_WORKSPACE_WRAPPER=str(driver), MODMAP_OUT=str(out))
    if not out.is_file():
        raise ModmapError(f"{out} was not written: the driver never compiled {crate / 'src/lib.rs'}")
    if out.stat().st_mtime_ns < start:
        raise ModmapError(f"{out} predates this run")
    rows = h2_depinfo.load_modmap(out)
    if (files := sum(row.kind == "file" for row in rows)) < min_modules:
        raise ModmapError(f"{out} lists {files} file modules (< {min_modules})")
    return rows

def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", type=Path, default=m.REPO_ROOT)
    parser.add_argument("--out", type=Path, help="map path (default <repo>/target/h2/modmap.tsv)")
    parser.add_argument("--inert", action="store_true", help="skip the repo map while no baseline is committed")
    parser.add_argument("--canary", action="store_true", help="self-test the driver even when --inert skips the map")
    args = parser.parse_args(argv)
    root = args.repo.resolve()
    skip_map = args.inert and not any((root / rel).exists() for rel in m.BASELINE_FILES)
    if skip_map and not args.canary:
        print("h2-modmap: no baseline committed; inert no-op")
        return 0
    out = args.out or root / "target/h2/modmap.tsv"
    try:
        driver = build_driver(root)
        canary = root / DRIVER / "canary"
        got = h2_depinfo.modmap_problems(map_modules(root, driver, canary, root / "target/h2/canary.tsv", 1,
                                                     "--locked", "--target-dir", str(root / "target/h2/canary")))
        if got != CANARY_PROBLEMS:
            raise ModmapError("driver canary drifted; got:\n  " + "\n  ".join(got or ["(no problems)"]))
        if skip_map:
            print("h2-modmap: driver canary holds; no baseline committed, repo map skipped")
            return 0
        rows = map_modules(root, driver, root, out, MIN_MODULES)
    except (ModmapError, m.MeasureError, OSError) as exc:
        print(f"h2-modmap: {exc}", file=sys.stderr)
        return 1
    print(f"h2-modmap: {sum(row.kind == 'file' for row in rows)} file modules -> {out}")
    return 0

if __name__ == "__main__":
    sys.exit(main())

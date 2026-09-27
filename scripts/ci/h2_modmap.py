#!/usr/bin/env python3
"""H2 R-O module map: build tools/modmap-driver, prove it on its canary, then map the root lib's file modules.

Only a complete map written by this run is accepted; scripts/ci/h2_depinfo.py judges it. With --inert and no
baseline the repo map is skipped, and so is everything else unless --canary asks for the driver self-test.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import subprocess
import sys
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import h2_depinfo  # noqa: E402
import h2_measure as m  # noqa: E402
import h2_cfg_compare as cfg_compare  # noqa: E402

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
CANARY_CFG = {
    ("feature", "h2_cfg_probe"), ("h2_probe_pair",), ("h2_probe_pair", ""),
    ("h2_probe_multi", "first"), ("h2_probe_multi", "second"),
    ("h2_probe_escape", 'quote=" slash=\\ newline=\n한글'),
}

class ModmapError(RuntimeError):
    pass

def cargo(root: Path, *args: str, log: Path | None = None, **env: str) -> list[dict]:
    """No rustc wrapper: sccache cannot wrap the driver, and a cache hit would skip writing the map."""
    full = {k: v for k, v in os.environ.items() if k not in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET")}
    full.pop("RUSTC_BOOTSTRAP", None)
    full.update(RUSTC_WRAPPER="", CARGO_BUILD_RUSTC_WRAPPER="", CARGO_INCREMENTAL="0", **env)
    proc = subprocess.run(["cargo", *args], cwd=root, env=full, capture_output=log is not None, text=True)
    events = []
    if log is not None:
        log.write_text(proc.stdout, encoding="utf-8")
        log.with_suffix(".stderr").write_text(proc.stderr, encoding="utf-8")
        print(proc.stderr, end="", file=sys.stderr)
        for line in proc.stdout.splitlines():
            try:
                event = json.loads(line)
            except ValueError:
                print(line, file=sys.stderr)
                continue
            if isinstance(event, dict):
                events.append(event)
                if event.get("reason") == "compiler-message":
                    print(event["message"].get("rendered") or event["message"]["message"], end="\n", file=sys.stderr)
    if proc.returncode:
        raise ModmapError(f"`cargo {' '.join(args)}` failed ({proc.returncode})")
    return events

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

def fresh(path: Path, start: int) -> None:
    if not path.is_file():
        raise ModmapError(f"{path} was not written: the driver did not publish this output")
    if path.stat().st_mtime_ns < start:
        raise ModmapError(f"{path} predates this run")

def check_canary_cfg(path: Path, nonce: str, driver: Path, crate: Path, events: list[dict]) -> None:
    atoms = cfg_compare.read_cfg(path)
    raw = json.loads(path.read_text(encoding="utf-8"))
    if raw != [list(atom) for atom in sorted(atoms)]:
        raise ModmapError("cfg canary: snapshot is not sorted and unique")
    probes = {atom for atom in atoms if atom[0].startswith("h2_probe_") or atom[0] == "feature"}
    if probes != CANARY_CFG:
        raise ModmapError(f"cfg canary: probe atoms differ: {sorted(probes)!r}")
    invocation = json.loads(Path(str(path) + ".invocation.json").read_text(encoding="utf-8"))
    if not isinstance(invocation, dict) or invocation.get("nonce") != nonce:
        raise ModmapError("cfg canary: driver nonce mismatch")
    argv = invocation.get("argv")
    if (not isinstance(argv, list) or not all(isinstance(arg, str) for arg in argv) or len(argv) < 3
            or argv[0] != str(driver) or invocation.get("root") != str(crate.resolve())
            or "--test" in argv or not any((crate / arg).resolve() == crate / "src/lib.rs" for arg in argv[2:])):
        raise ModmapError("cfg canary: driver argv/root mismatch")
    explicit = {argv[i + 1] for i, arg in enumerate(argv[:-1]) if arg == "--cfg"}
    expected = {atom[0] + ("=" + json.dumps(atom[1], ensure_ascii=False) if len(atom) == 2 else "")
                for atom in CANARY_CFG}
    if explicit != expected or "--cfg" == argv[-1]:
        raise ModmapError("cfg canary: driver --cfg argv mismatch")
    os_name = {"linux": "linux", "darwin": "macos"}.get(sys.platform)
    arch = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64"}.get(platform.machine())
    session = {("target_os", os_name), ("target_arch", arch), ("target_family", "unix"),
               ("unix",), ("panic", "unwind"), ("debug_assertions",)}
    names = {atom[0] for atom in session} | {"test", "windows"}
    if not os_name or not arch or {atom for atom in atoms if atom[0] in names} != session:
        raise ModmapError("cfg canary: session target/codegen atoms differ (test/windows must be absent)")
    supplied = explicit | {cfg for event in events if event.get("reason") == "build-script-executed"
                           for cfg in event.get("cfgs", [])}
    supplied_names = {cfg.partition("=")[0] for cfg in supplied}
    if not any(atom[0] not in supplied_names for atom in session):
        raise ModmapError("cfg canary: no session-only atom outside Cargo events/argv")
    if cfg_compare.compare_cfgs(atoms, atoms) != {"common": sorted(atoms), "linux_only": [], "macos_only": []}:
        raise ModmapError("cfg canary: identical comparison failed")
    for atom in CANARY_CFG | session:
        if cfg_compare.compare_cfgs(atoms, atoms - {atom}) != {
            "common": sorted(atoms - {atom}), "linux_only": [atom], "macos_only": [],
        }:
            raise ModmapError(f"cfg canary: removed atom not detected: {atom!r}")
    print(f"h2-modmap: cfg self-test holds ({len(atoms)} atoms; driver nonce/argv and session-only cfg verified)")

def map_modules(root: Path, driver: Path, crate: Path, out: Path, min_modules: int, *extra: str,
                cfg_out: Path | None = None) -> list:
    """The driver's rows for `crate`'s lib; a map this run did not write, or wrote short, is an error."""
    out.parent.mkdir(parents=True, exist_ok=True)
    outputs = [out] if cfg_out is None else [out, cfg_out, Path(str(cfg_out) + ".invocation.json")]
    for path in outputs:
        path.unlink(missing_ok=True)
    marker = out.with_name(out.name + ".start")
    marker.touch()
    start = marker.stat().st_mtime_ns  # the file clock, which also stamps the map
    nonce = uuid.uuid4().hex
    events = cargo(root, "check", "--lib", "-q", "--message-format=json", "--manifest-path", str(crate / "Cargo.toml"), *extra,
                   log=out.with_suffix(".cargo.jsonl"), RUSTC_WORKSPACE_WRAPPER=str(driver), MODMAP_OUT=str(out),
                   MODMAP_CFG_OUT=str(cfg_out) if cfg_out else "", MODMAP_CFG_NONCE=nonce)
    for path in outputs:
        fresh(path, start)
    rows = h2_depinfo.load_modmap(out)
    if (files := sum(row.kind == "file" for row in rows)) < min_modules:
        raise ModmapError(f"{out} lists {files} file modules (< {min_modules})")
    if cfg_out is not None:
        check_canary_cfg(cfg_out, nonce, driver, crate, events)
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
                                                     "--locked", "--target-dir", str(root / "target/h2/canary"),
                                                     "--features", "h2_cfg_probe", cfg_out=root / "target/h2/canary.cfg.json"))
        if got != CANARY_PROBLEMS:
            raise ModmapError("driver canary drifted; got:\n  " + "\n  ".join(got or ["(no problems)"]))
        if skip_map:
            print("h2-modmap: driver canary holds; no baseline committed, repo map skipped")
            return 0
        rows = map_modules(root, driver, root, out, MIN_MODULES)
    except (ModmapError, m.MeasureError, OSError, ValueError) as exc:
        print(f"h2-modmap: {exc}", file=sys.stderr)
        return 1
    print(f"h2-modmap: {sum(row.kind == 'file' for row in rows)} file modules -> {out}")
    return 0

if __name__ == "__main__":
    sys.exit(main())

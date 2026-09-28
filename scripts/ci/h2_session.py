"""Collect and seal an opt-in, cfg-only Clippy wrapper session."""
from __future__ import annotations

import json
import os
import re
import subprocess
import uuid
from pathlib import Path

import h2_cfg_collect as collect
import h2_env
import h2_modmap as modmap
from h2_measure import LINTS, RO_LINTS, MeasureError

ALLOWED = dict(release="1.94.1", commit="e408947bfd200af42db322daf0fadfe7e26d3bd1", cargo="cargo 1.94.1 ",
               clippy="clippy 0.1.94 (e408947bfd ", driver_rustc="1.94.1 (e408947bf 2026-03-25)")
SCHEMA = "h2-session/1-cfg"
OUTPUTS = ("session.json", "session.json.claim", "session.json.items-cfg.txt", "session.json.clippy-cfg.txt",
           "session.json.items.stdout", "session.json.items.stderr", "session.json.probe.stdout",
           "session.json.probe.stderr", "clippy.jsonl", "cargo.stderr", "cargo.json")


def output(argv: list[str], cwd: Path, env: dict) -> str:
    proc = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True)
    if proc.returncode:
        raise MeasureError(f"session toolchain/request command failed: {argv} ({proc.returncode})")
    return proc.stdout.strip()


def toolchain_guard(crate: Path, env: dict, driver: Path, clippy: Path | None = None) -> dict:
    def query(*args):
        return output(list(args), crate, env)
    def fields(value):
        return dict(line.split(": ", 1) for line in value.splitlines() if ": " in line)
    rustc = query("rustc", "-vV")
    rv = fields(rustc)
    if rv.get("release") != ALLOWED["release"] or rv.get("commit-hash") != ALLOWED["commit"]:
        raise MeasureError("unsupported toolchain: rustc release/commit")
    cargo = query("cargo", "-V")
    if not cargo.startswith(ALLOWED["cargo"]):
        raise MeasureError("unsupported toolchain: Cargo version")
    sysroot = Path(query("rustc", "--print", "sysroot"))
    if not sysroot.is_absolute():
        raise MeasureError("unsupported toolchain: invalid sysroot")
    want = (sysroot / "bin/clippy-driver").resolve(strict=True)
    clippy = clippy.resolve(strict=True) if clippy is not None else want
    if clippy != want:
        raise MeasureError("unsupported toolchain: clippy-driver is outside the active sysroot")
    cv, cr = query(str(clippy), "--version"), query(str(clippy), "--rustc", "-vV")
    if not cv.startswith(ALLOWED["clippy"]) or fields(cr).get("commit-hash") != ALLOWED["commit"]:
        raise MeasureError("unsupported toolchain: clippy identity")
    dv = query(str(driver), "__modmap_version")
    if dv != ALLOWED["driver_rustc"]:
        raise MeasureError("unsupported toolchain: modmap-driver compiler")
    return dict(rustc=rustc, cargo=cargo, clippy=cv, clippy_rustc=cr, clippy_driver=str(clippy), driver_rustc=dv)


def requested_unit(crate: Path, env: dict) -> dict:
    metadata = json.loads(output(["cargo", "metadata", "--no-deps", "--offline", "--format-version", "1"], crate, env))
    manifest = (crate / "Cargo.toml").resolve(strict=True)
    packages = [p for p in metadata["packages"] if Path(p["manifest_path"]).resolve() == manifest]
    if len(packages) != 1:
        raise MeasureError("session request: expected exactly one package at the requested manifest")
    package = packages[0]
    libs = [t for t in package["targets"] if set(t["kind"]) & {"lib", "rlib", "cdylib", "dylib", "staticlib", "proc-macro"}]
    if len(libs) != 1 or "proc-macro" in libs[0]["kind"]:
        raise MeasureError("session request: expected one non-proc-macro lib")
    lib = libs[0]
    types = sorted(lib["crate_types"])
    if not types or set(types) & {"bin", "proc-macro"}:
        raise MeasureError("session request: unsupported crate types")
    return dict(manifest=str(manifest), package=package["name"], package_id=package["id"],
                lib=str(Path(lib["src_path"]).resolve(strict=True)), crate_name=lib["name"].replace("-", "_"), crate_types=types)


def source_state(root: Path, lib: Path, conf: Path) -> dict:
    return dict(repo=modmap.source_state(root), lib=collect.digest(lib.read_bytes()),
                config=collect.digest(collect.regular(conf / "clippy.toml")))


def validate(run: Path, request: dict) -> dict:
    if list(run.glob("*.partial")):
        raise MeasureError("session has partial outputs")
    start = (run / "start").stat().st_mtime_ns
    request_bytes = collect.regular(run / "request.json", start)
    if json.loads(request_bytes) != request:
        raise MeasureError("session request changed")
    if not (run / "session.json").is_file():
        raise MeasureError("no root compile in this session (Cargo replayed a fresh unit)")
    data = {name: collect.regular(run / name, start) for name in OUTPUTS}
    proof = json.loads(data["session.json"])
    try:
        claim = json.loads(data["session.json.claim"])
        if (type(proof.get("pid")) is not int or proof["pid"] <= 0 or not isinstance(claim, dict)
                or type(claim.get("pid")) is not int or claim["pid"] != proof["pid"] or claim.get("unit") != proof.get("unit")):
            raise ValueError("identity differs")
    except (ValueError, AttributeError) as exc:
        raise MeasureError(f"session claim: {exc}") from exc
    if proof.get("schema") != SCHEMA:
        raise MeasureError("session proof schema mismatch")
    for key in ("nonce", "run_id"):
        if proof.get(key) != request[key]:
            raise MeasureError(f"session {key} mismatch")
    unit, expected = proof.get("unit"), request["unit"]
    if (not isinstance(unit, dict) or any(unit.get(k) != expected[k] for k in
            ("manifest", "package", "lib", "crate_name", "crate_types")) or unit.get("test") is not False
            or unit.get("root") != str(Path(expected["manifest"]).parent) or not isinstance(unit.get("metadata"), str)):
        raise MeasureError("session unit identity mismatch")
    events = [json.loads(line) for line in data["clippy.jsonl"].splitlines() if line.strip()]
    if any(not isinstance(e, dict) for e in events):
        raise MeasureError("session artifact: invalid Cargo JSONL")
    artifacts = [e for e in events if e.get("reason") == "compiler-artifact"
                 and Path(e["target"]["src_path"]).resolve() == Path(expected["lib"]) and e["profile"]["test"] is False]
    if len(artifacts) != 1 or artifacts[0]["package_id"] != expected["package_id"] or artifacts[0]["fresh"] is not False:
        raise MeasureError("session artifact: expected one fresh:false artifact for the requested lib")
    cfg = proof.get("cfg")
    if (not isinstance(cfg, list) or not all(isinstance(v, str) for v in cfg) or "clippy" not in cfg
            or data["session.json.items-cfg.txt"] != data["session.json.clippy-cfg.txt"]
            or data["session.json.items-cfg.txt"] != ("\n".join(cfg) + "\n").encode()):
        raise MeasureError("session cfg mismatch")
    if proof.get("driver_rustc") != request["toolchain"]["driver_rustc"]:
        raise MeasureError("session driver compiler mismatch")
    if not isinstance(proof.get("env_sha256"), str) or not re.fullmatch(r"[0-9a-f]{64}", proof["env_sha256"]):
        raise MeasureError("session env digest missing/invalid")
    argv = proof.get("argv")
    if (not isinstance(argv, list) or not all(isinstance(a, str) for a in argv) or len(argv) < 2 or "--test" in argv):
        raise MeasureError("session argv mismatch")
    for i, arg in enumerate(argv):
        if arg == "--target" or arg.startswith("--target="):
            target = argv[i + 1] if arg == "--target" and i + 1 < len(argv) else arg.removeprefix("--target=")
            if target != request["target"]:
                raise MeasureError("session argv target mismatch")
    if json.loads(data["cargo.json"])["rc"] != 0:
        raise MeasureError("session Cargo failed")
    if source_state(Path(request["repo"]), Path(expected["lib"]), Path(request["conf_dir"])) != request["source"]:
        raise MeasureError("session source changed")
    data["request.json"] = request_bytes
    return dict(schema=collect.SCHEMA, kind="canary-cfg", manifest=str(run / "manifest.json"), run_dir=str(run),
                root=unit["root"], lane=request["lane"], run_id=request["run_id"], nonce=request["nonce"],
                request=request, proof=proof, digests={name: collect.digest(body) for name, body in data.items()})


def session(root: Path, crate: Path, run_dir: Path, conf_dir: Path, lane: str, *, extra=(),
            driver: Path | None = None, clippy: Path | None = None) -> dict:
    try:
        root, crate, conf_dir = root.resolve(strict=True), crate.resolve(strict=True), conf_dir.resolve(strict=True)
        run = run_dir.absolute()
        if run != run.resolve() or (run.exists() and (not run.is_dir() or any(run.iterdir()))):
            raise MeasureError("session run directory must be new or empty and canonical")
        driver = (driver or root / "target/modmap-driver/release/modmap-driver").resolve(strict=True)
        env = {k: v for k, v in h2_env.environment("measure").items() if not k.startswith("MODMAP_")}
        toolchain = toolchain_guard(crate, env, driver, clippy)
        host = h2_env.check_host(lane, toolchain["rustc"])
        unit = requested_unit(crate, env)
        run.mkdir(parents=True, exist_ok=True)
        (run / "start").touch()
        nonce, run_id = uuid.uuid4().hex, uuid.uuid4().hex
        request = dict(schema=SCHEMA, kind="canary-cfg", unit=unit, repo=str(root), conf_dir=str(conf_dir),
                       run_id=run_id, nonce=nonce, lane=lane, host=host, target=h2_env.LANES[lane], toolchain=toolchain,
                       source=source_state(root, Path(unit["lib"]), conf_dir))
        collect.write_json(run / "request.json", request)
        flags = ["--cap-lints", "warn"] + [v for lint in (*LINTS, *RO_LINTS) for v in ("--force-warn", lint)]
        env.update(RUSTC_WORKSPACE_WRAPPER=str(driver), MODMAP_CLIPPY_DRIVER=toolchain["clippy_driver"],
                   CLIPPY_ARGS="__CLIPPY_HACKERY__".join([*flags, ""]), CLIPPY_TERMINAL_WIDTH="0",
                   CLIPPY_CONF_DIR=str(conf_dir), MODMAP_SESSION_OUT=str(run / "session.json"),
                   MODMAP_CFG_NONCE=nonce, MODMAP_RUN_ID=run_id,
                   **{f"MODMAP_EXPECT_{k.upper()}": unit[k] for k in ("manifest", "package", "lib")})
        os.utime(unit["lib"], None)
        argv = ["cargo", "check", "--lib", "--message-format=json", *extra]
        proc = subprocess.run(argv, cwd=crate, env=env, capture_output=True, text=True)
        (run / "clippy.jsonl").write_text(proc.stdout, encoding="utf-8")
        (run / "cargo.stderr").write_text(proc.stderr, encoding="utf-8")
        collect.write_json(run / "cargo.json", dict(rc=proc.returncode, argv=argv))
        if proc.returncode:
            raise MeasureError(f"session Cargo failed ({proc.returncode}); see {run / 'cargo.stderr'}")
        manifest = validate(run, request)
        collect.write_json(run / "manifest.json", manifest)
        return manifest
    except (OSError, ValueError, KeyError, TypeError, AttributeError, h2_env.HostMismatch) as exc:
        raise MeasureError(f"session: {exc}") from exc

"""Collect and seal compiler items from a Clippy wrapper session."""
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import time
import tomllib
import uuid
from pathlib import Path

import h2_cfg_collect as collect
import h2_env
import h2_modmap as modmap
from h2_measure import LINTS, RO_LINTS, MeasureError

ALLOWED = dict(release="1.94.1", commit="e408947bfd200af42db322daf0fadfe7e26d3bd1", cargo="cargo 1.94.1 ",
               clippy="clippy 0.1.94 (e408947bfd ", driver_rustc="1.94.1 (e408947bf 2026-03-25)")
SCHEMA = "h2-session/2"
PROTECTED_ENV = ("CLIPPY_ARGS", "CLIPPY_CONF_DIR", "CLIPPY_TERMINAL_WIDTH", "MODMAP_SESSION_OUT",
                 "MODMAP_CFG_NONCE", "MODMAP_RUN_ID", "MODMAP_EXPECT_MANIFEST", "MODMAP_EXPECT_PACKAGE", "MODMAP_EXPECT_LIB")
TARGET_CFG = {"x86_64-unknown-linux-gnu": ("x86_64", "linux", "gnu", "unknown", "64"),
              "aarch64-apple-darwin": ("aarch64", "macos", "", "apple", "64")}
LIB_KINDS = {"lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"}
TARGET_KEYS = ("target_arch", "target_os", "target_env", "target_vendor", "target_pointer_width")
OUTPUTS = ("session.json", "session.json.claim", "session.json.items-cfg.txt", "session.json.clippy-cfg.txt",
           "session.json.items.stdout", "session.json.items.stderr", "session.json.probe.stdout",
           "session.json.probe.stderr", "clippy.jsonl", "cargo.stderr", "cargo.json", "items.jsonl", "items.jsonl.sha256")


def validate_items(data: dict, proof: dict, request: dict) -> None:
    try:
        body = data["items.jsonl"]
        rows = [json.loads(line) for line in body.splitlines()]
        receipt = json.loads(data["items.jsonl.sha256"])
        count, digest = proof.get("items_records"), proof.get("items_sha256")
        if (type(count) is not int or count <= 0 or count != len(rows) - 1 or not body.endswith(b"\n")
                or digest != collect.digest(body) or receipt != dict(sha256=digest, records=count)
                or type(receipt.get("records")) is not int):
            raise ValueError("callback/file/proof digest or record count mismatch")
        want = dict(schema=1, kind=request["kind"], root=str(Path(request["unit"]["manifest"]).parent),
                    crate=request["unit"]["crate_name"], nonce=request["nonce"], run_id=request["run_id"], cfg_clippy=True)
        if rows[0] != want or type(rows[0].get("schema")) is not int or rows[0].get("cfg_clippy") is not True:
            raise ValueError("header mismatch")
        modmap.item_records(rows[1:])
    except (ValueError, TypeError, AttributeError) as exc:
        raise MeasureError(f"session items: {exc}") from exc


def check_extra(extra) -> None:
    values = {"--features", "-F", "--jobs", "-j", "--target", "--target-dir"}
    switches = {"--locked", "--offline", "--frozen", "--all-features", "--no-default-features",
                "--all-targets", "--verbose", "-v", "--quiet", "-q"}
    args = iter(extra)
    for arg in args:
        flag, equals, value = arg.partition("=")
        if flag in switches and not equals:
            continue
        value = value if equals else next(args, "")
        if flag not in values or not value or value.startswith(("-", "@", "+")):
            raise MeasureError(f"session extra option is unsupported: {arg}")


def cargo_config(crate: Path, env: dict) -> dict:
    home = Path(env.get("CARGO_HOME") or Path.home() / ".cargo")
    directories = {p / ".cargo" for p in (crate, *crate.parents)} | {(crate / home).resolve()}
    reserved = set(h2_env.CLEAR) | {"PATH", "HOME", "CARGO_HOME", "CARGO_INCREMENTAL",
                                   "CARGO_MANIFEST_DIR", "CARGO_PKG_NAME", "LD_LIBRARY_PATH",
                                   "DYLD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH"}
    files = {}
    for directory in sorted(directories):
        for name in ("config", "config.toml"):
            path = directory / name
            if not path.exists():
                continue
            body = path.read_bytes()
            config = tomllib.loads(body.decode("utf-8"))
            if "include" in config:
                raise MeasureError(f"session Cargo config includes are unsupported: {path}")
            overlays = [config.get("env", {})]
            for target in config.get("target", {}).values():
                overlays.extend(value.get("rustc-env", {}) for value in target.values() if isinstance(value, dict))
            for key in {key for overlay in overlays for key in overlay}:
                if (key in reserved or key.startswith(("MODMAP_", "CLIPPY_", "RUSTC", "RUSTUP_"))
                        or h2_env.CLEAR_RE.fullmatch(key)):
                    raise MeasureError(f"session reserved Cargo env key {key}: {path}")
            if any(config.get("build", {}).get(key) for key in ("rustc", "rustc-wrapper", "rustc-workspace-wrapper")):
                raise MeasureError(f"session reserved Cargo compiler setting: {path}")
            files[str(path)] = collect.digest(body)
    return files


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
                lib=str(Path(lib["src_path"]).resolve(strict=True)), crate_name=lib["name"].replace("-", "_"), crate_types=types,
                workspace_root=metadata["workspace_root"])


def source_state(root: Path, lib: Path, conf: Path) -> dict:
    return source_capture(root, lib, conf)[0]


def source_capture(root: Path, lib: Path, conf: Path) -> tuple[dict, dict[str, bytes], dict[str, list[int]]]:
    """source_state, the absolute-path bytes it digests and their stats; a listed lib is not reread."""
    try:
        repo, bodies, stats = modmap.source_capture(root)
        files = {str(root / name): body for name, body in bodies.items()}
        stats = {str(root / name): stat for name, stat in stats.items()}
        config = conf / "clippy.toml"
        if config.absolute() != config.resolve() or not config.is_file():
            raise MeasureError(f"session config {config} is not a regular canonical file")
        for path in (lib, config):
            if str(path) not in files:
                files[str(path)], stats[str(path)] = modmap.read_stable(path)
    except modmap.ModmapError as exc:
        raise MeasureError(f"session source capture: {exc}") from exc
    return (dict(repo=repo, lib=collect.digest(files[str(lib)]), config=collect.digest(files[str(config)])),
            files, stats)


def probe_ctime(fd: int) -> int:
    return os.fstat(fd).st_ctime_ns


def clock_probe(path: Path, *, advances: int = 2, budget: float = 5.0) -> dict:
    """Rewrite one unlisted file until its ctime advances; a later write to any fenced file then gets a later ctime."""
    seen, writes, deadline = [], 0, time.monotonic() + budget
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        while len(seen) <= advances and time.monotonic() < deadline:
            os.pwrite(fd, b"fence\n", 0)
            writes, stamp = writes + 1, probe_ctime(fd)
            if seen and stamp < seen[-1]:
                raise MeasureError("session fence: ctime went backwards")
            if not seen or stamp != seen[-1]:
                seen.append(stamp)
        dev = os.fstat(fd).st_dev
    finally:
        os.close(fd)
        path.unlink()
    probe = dict(dev=dev, ctimes=seen, writes=writes, same_tick=writes - len(seen))
    check_probe(probe)
    return probe


def check_probe(probe) -> int:
    """The finest ctime step the probe saw; no advance or whole-second stamps cannot separate write and restore."""
    ctimes = probe.get("ctimes") if isinstance(probe, dict) else None
    if (not isinstance(ctimes, list) or len(ctimes) < 3 or any(type(c) is not int for c in ctimes)
            or type(probe.get("dev")) is not int or any(b <= a for a, b in zip(ctimes, ctimes[1:]))):
        raise MeasureError(f"session fence: ctime did not advance on this filesystem ({probe!r})")
    step = min(b - a for a, b in zip(ctimes, ctimes[1:]))
    if all(c % 1_000_000_000 == 0 for c in ctimes) or step >= 1_000_000_000:
        raise MeasureError(f"session fence: ctime resolution is whole seconds ({ctimes}); refusing the fence")
    return step


def check_fence(fence, files_read: dict[str, bytes]) -> None:
    """The sealed stats cover exactly the captured files, on the probed filesystem, all older than the probe."""
    if not isinstance(fence, dict) or not isinstance(fence.get("files"), dict) or not fence["files"]:
        raise MeasureError("session fence: no stat list")
    check_probe(fence.get("probe"))
    files, probe = fence["files"], fence["probe"]
    if set(files) != set(files_read):
        raise MeasureError("session fence: stat list differs from the captured files")
    for name, stat in files.items():
        if not isinstance(stat, list) or len(stat) != len(modmap.STAT) or any(type(v) is not int for v in stat):
            raise MeasureError(f"session fence: malformed stat for {name}")
        if stat[2] != len(files_read[name]):
            raise MeasureError(f"session fence: {name} stat size differs from its captured bytes")
        if stat[0] != probe["dev"]:
            raise MeasureError(f"session fence: {name} is not on the probed filesystem")
        if stat[4] >= probe["ctimes"][-1]:
            raise MeasureError(f"session fence: {name} changed after the clock probe")


def fence_marker(fence: dict) -> dict:
    return dict(held=True, files=collect.digest(json.dumps(fence["files"], sort_keys=True).encode()),
                resolution_ns=check_probe(fence["probe"]))


def check_fence_end(fence: dict) -> None:
    """Any write since capture moved ctime, and a replacement moved the inode, even if the bytes came back."""
    for name, stat in sorted(fence["files"].items()):
        try:
            now = modmap.stat_of(name)
        except OSError:
            now = None
        if now != stat:
            fields = "removed" if now is None else ",".join(f for f, a, b in zip(modmap.STAT, stat, now) if a != b)
            raise MeasureError(f"session fence: {name} was written during Cargo ({fields})")


def resolver(fail=MeasureError):
    """Path.resolve memoized per spelling, so a link swapped mid-check cannot reclassify a target."""
    resolved: dict[str, Path] = {}
    def canon(name: str) -> Path:
        if name not in resolved:
            try:
                resolved[name] = Path(name).resolve()
            except RuntimeError as exc:
                raise fail(f"cannot resolve {name}: {exc}") from exc
        return resolved[name]
    return canon


def lib_artifacts(events: list[dict], package_id: str, lib: str, canon) -> list[dict]:
    """Artifacts of the package's lib target at lib; a build script or bin sharing that file is not one."""
    return [e for e in events if e.get("reason") == "compiler-artifact" and e.get("package_id") == package_id
            and set(e["target"].get("kind") or ()) & LIB_KINDS and canon(e["target"]["src_path"]) == Path(lib)]


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
    validate_items(data, proof, request)
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
    artifacts = [e for e in lib_artifacts(events, expected["package_id"], expected["lib"], resolver())
                 if e["profile"]["test"] is False]
    if len(artifacts) != 1 or artifacts[0]["fresh"] is not False:
        raise MeasureError("session artifact: expected one fresh:false artifact for the requested lib")
    cfg = proof.get("cfg")
    if (not isinstance(cfg, list) or not all(isinstance(v, str) for v in cfg) or "clippy" not in cfg
            or data["session.json.items-cfg.txt"] != data["session.json.clippy-cfg.txt"]
            or data["session.json.items-cfg.txt"] != ("\n".join(cfg) + "\n").encode()):
        raise MeasureError("session cfg mismatch")
    target_cfg = {f'{key}="{value}"' for key, value in zip(TARGET_KEYS, TARGET_CFG[request["target"]])}
    if {entry for entry in cfg if entry.partition("=")[0] in TARGET_KEYS} != target_cfg:
        raise MeasureError("session cfg target mismatch")
    for key in ("clippy_driver", "clippy", "clippy_rustc"):
        if proof.get(key) != request["toolchain"][key]:
            raise MeasureError(f"session clippy identity mismatch: {key}")
    if proof.get("driver_rustc") != request["toolchain"]["driver_rustc"]:
        raise MeasureError("session driver compiler mismatch")
    protected = request.get("protected_env")
    if (not isinstance(protected, dict) or set(protected) != set(PROTECTED_ENV)
            or not all(isinstance(value, str) for value in protected.values()) or proof.get("protected_env") != protected):
        raise MeasureError("session protected env mismatch")
    if not isinstance(proof.get("env_sha256"), str) or not re.fullmatch(r"[0-9a-f]{64}", proof["env_sha256"]):
        raise MeasureError("session env digest missing/invalid")
    argv = proof.get("argv")
    if (not isinstance(argv, list) or not all(isinstance(a, str) for a in argv) or len(argv) < 2 or "--test" in argv):
        raise MeasureError("session argv mismatch")
    if any(arg.startswith("@") for arg in argv):
        raise MeasureError("session response-file compiler arguments are unsupported")
    for i, arg in enumerate(argv):
        if arg == "--target" or arg.startswith("--target="):
            target = argv[i + 1] if arg == "--target" and i + 1 < len(argv) else arg.removeprefix("--target=")
            if target != request["target"]:
                raise MeasureError("session argv target mismatch")
    if json.loads(data["cargo.json"])["rc"] != 0:
        raise MeasureError("session Cargo failed")
    check_fence_end(request["fence"])
    state, files, _ = source_capture(Path(request["repo"]), Path(expected["lib"]), Path(request["conf_dir"]))
    if state != request["source"]:
        raise MeasureError("session source changed")
    check_fence(request["fence"], files)
    data["request.json"] = request_bytes
    return dict(schema=collect.SCHEMA, kind="canary-items", manifest=str(run / "manifest.json"), run_dir=str(run),
                root=unit["root"], lane=request["lane"], run_id=request["run_id"], nonce=request["nonce"],
                request=request, proof=proof, fence=fence_marker(request["fence"]),
                digests={name: collect.digest(body) for name, body in data.items()})


def session(root: Path, crate: Path, run_dir: Path, conf_dir: Path, lane: str, *, extra=(),
            driver: Path | None = None, clippy: Path | None = None) -> dict:
    try:
        root, crate, conf_dir = root.resolve(strict=True), crate.resolve(strict=True), conf_dir.resolve(strict=True)
        run = run_dir.absolute()
        if run != run.resolve() or (run.exists() and (not run.is_dir() or any(run.iterdir()))):
            raise MeasureError("session run directory must be new or empty and canonical")
        driver = (driver or root / "target/modmap-driver/release/modmap-driver").resolve(strict=True)
        env = {k: v for k, v in h2_env.environment("measure").items() if not k.startswith("MODMAP_")}
        extra = tuple(extra)
        check_extra(extra)
        configs = cargo_config(crate, env)
        toolchain = toolchain_guard(crate, env, driver, clippy)
        host = h2_env.check_host(lane, toolchain["rustc"])
        unit = requested_unit(crate, env)
        run.mkdir(parents=True, exist_ok=True)
        (run / "start").touch()
        nonce, run_id = uuid.uuid4().hex, uuid.uuid4().hex
        os.utime(unit["lib"], None)
        probe = clock_probe(run / "fence.probe")
        print(f"h2-session: ctime fence probe {probe['writes']} writes, step {check_probe(probe)} ns, "
              f"{probe['same_tick']} same-tick rewrites", file=sys.stderr)
        state, files, stats = source_capture(root, Path(unit["lib"]), conf_dir)
        fence = dict(files=stats, probe=probe)
        check_fence(fence, files)
        request = dict(schema=SCHEMA, kind="canary-items", unit=unit, repo=str(root), conf_dir=str(conf_dir),
                       run_id=run_id, nonce=nonce, lane=lane, host=host, target=h2_env.LANES[lane], toolchain=toolchain,
                       cargo_config=configs, source=state, fence=fence)
        flags = ["--cap-lints", "warn"] + [v for lint in (*LINTS, *RO_LINTS) for v in ("--force-warn", lint)]
        env.update(RUSTC_WORKSPACE_WRAPPER=str(driver), MODMAP_CLIPPY_DRIVER=toolchain["clippy_driver"],
                   CLIPPY_ARGS="__CLIPPY_HACKERY__".join([*flags, ""]), CLIPPY_TERMINAL_WIDTH="0",
                   CLIPPY_CONF_DIR=str(conf_dir), MODMAP_SESSION_OUT=str(run / "session.json"),
                   MODMAP_CFG_NONCE=nonce, MODMAP_RUN_ID=run_id,
                   **{f"MODMAP_EXPECT_{k.upper()}": unit[k] for k in ("manifest", "package", "lib")})
        request["protected_env"] = {key: env[key] for key in PROTECTED_ENV}
        collect.write_json(run / "request.json", request)
        if cargo_config(crate, env) != configs:
            raise MeasureError("session Cargo config changed")
        argv = ["cargo", "check", "--lib", "--message-format=json", "--package", unit["package_id"], *extra]
        proc = subprocess.run(argv, cwd=crate, env=env, capture_output=True, text=True)
        (run / "clippy.jsonl").write_text(proc.stdout, encoding="utf-8")
        (run / "cargo.stderr").write_text(proc.stderr, encoding="utf-8")
        collect.write_json(run / "cargo.json", dict(rc=proc.returncode, argv=argv))
        if proc.returncode:
            raise MeasureError(f"session Cargo failed ({proc.returncode}); see {run / 'cargo.stderr'}")
        if cargo_config(crate, env) != configs:
            raise MeasureError("session Cargo config changed")
        manifest = validate(run, request)
        collect.write_json(run / "manifest.json", manifest)
        return manifest
    except (OSError, ValueError, KeyError, TypeError, AttributeError, h2_env.HostMismatch) as exc:
        raise MeasureError(f"session: {exc}") from exc

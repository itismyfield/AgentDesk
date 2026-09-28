"""Collect and seal compiler items from a Clippy wrapper session."""
from __future__ import annotations

import ctypes
import errno
import json
import os
import re
import stat as S
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


def canonical(path, *, strict: bool = False, fail=MeasureError) -> Path:
    """Path.resolve with a link loop (RuntimeError before Python 3.13) or an OS error reported as fail."""
    try:
        return Path(path).resolve(strict=strict)
    except (RuntimeError, OSError) as exc:
        raise fail(f"cannot resolve {path}: {exc}") from exc


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
    directories = {p / ".cargo" for p in (crate, *crate.parents)} | {canonical(crate / home)}
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
    want = canonical(sysroot / "bin/clippy-driver", strict=True)
    clippy = canonical(clippy, strict=True) if clippy is not None else want
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
    manifest = canonical(crate / "Cargo.toml", strict=True)
    packages = [p for p in metadata["packages"] if canonical(p["manifest_path"]) == manifest]
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
                lib=str(canonical(lib["src_path"], strict=True)), crate_name=lib["name"].replace("-", "_"), crate_types=types,
                workspace_root=metadata["workspace_root"],
                target_dir=metadata.get("target_directory", str(Path(metadata["workspace_root"]) / "target")))


def source_state(root: Path, lib: Path, conf: Path) -> dict:
    return source_capture(root, lib, conf)[0]


def source_capture(root: Path, lib: Path, conf: Path) -> tuple[dict, dict[str, bytes], dict[str, list[int]]]:
    """source_state, the absolute-path bytes it digests and their stats; a listed lib is not reread."""
    try:
        repo, bodies, stats = modmap.source_capture(root)
        files = {str(root / name): body for name, body in bodies.items()}
        stats = {str(root / name): stat for name, stat in stats.items()}
        config = conf / "clippy.toml"
        if config.absolute() != canonical(config) or not config.is_file():
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


def fence_dirs(root: Path, name: str) -> list[Path]:
    """The directories a captured path is looked up through: from the workspace root, or its own parent if outside."""
    path = Path(name)
    anchor = root if path.is_relative_to(root) else path.parent
    return [d for d in path.parents if d.is_relative_to(anchor)]


def dir_stat(path) -> list[int]:
    st = os.lstat(path)
    if not S.S_ISDIR(st.st_mode):
        raise MeasureError(f"session fence: {path} is not a directory (a symlink?)")
    return [st.st_dev, st.st_ino, st.st_mtime_ns, st.st_ctime_ns]


def seal_dirs(root: Path, stats: dict[str, list[int]]) -> dict[str, list[int]]:
    """Each captured file is the regular file reached without a symlink; its lookup directories are sealed."""
    dirs = {}
    for name, stat in stats.items():
        leaf = os.lstat(name)
        if not S.S_ISREG(leaf.st_mode) or [leaf.st_dev, leaf.st_ino] != stat[:2]:
            raise MeasureError(f"session fence: {name} is not the regular file that was captured (a symlink?)")
        for directory in fence_dirs(root, name):
            dirs.setdefault(str(directory), dir_stat(directory))
    return dirs


class Region(ctypes.Structure):
    _fields_ = ([(n, ctypes.c_uint32) for n in ("prot", "max_prot", "inherit", "flags")] + [("offset", ctypes.c_uint64)]
                + [(f"u{i}", ctypes.c_uint32) for i in range(14)] + [("address", ctypes.c_uint64), ("size", ctypes.c_uint64)]
                + [("dev", ctypes.c_uint32), ("mode", ctypes.c_uint16), ("nlink", ctypes.c_uint16), ("ino", ctypes.c_uint64)]
                + [("vstat", ctypes.c_uint8 * 120), ("vtype", ctypes.c_int32 * 4), ("path", ctypes.c_char * 1024)])


class ShortInfo(ctypes.Structure):
    _fields_ = [(n, ctypes.c_uint32) for n in ("pid", "ppid", "pgid", "status")] + [("comm", ctypes.c_char * 16)] + \
        [(n, ctypes.c_uint32) for n in ("flags", "uid", "gid", "ruid", "rgid", "svuid", "svgid", "rfu")]


def libproc():
    return ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)


PROC_ROOT, PF_KTHREAD = Path("/proc"), 0x00200000


def list_pids() -> list[int]:
    if sys.platform == "linux":
        return [int(entry.name) for entry in os.scandir(PROC_ROOT) if entry.name.isdigit()]
    lib = libproc()
    size = lib.proc_listpids(1, 0, None, 0)
    buf = (ctypes.c_int * (size // 4 + 1024))()
    size = lib.proc_listpids(1, 0, buf, ctypes.sizeof(buf))
    if size <= 0:
        raise MeasureError("session fence: cannot list processes")
    return [pid for pid in buf[:size // 4] if pid > 0]


def task_view(task: Path):
    """(uids, state, maps bytes or None if unreadable) of one Linux task."""
    status = (task / "status").read_bytes()
    uids = {int(v) for v in re.search(rb"^Uid:(.*)$", status, re.M).group(1).split()}
    state = re.search(rb"^State:\s*(\S)", status, re.M).group(1)
    try:
        return uids, state, (task / "maps").read_bytes()
    except PermissionError:
        return uids, state, None


def linux_tasks(base: Path) -> dict:
    """Every task of the thread group, listed again until no new tid appears; a task gone meanwhile is None."""
    tasks = {}
    for _ in range(10):
        fresh = {entry.name for entry in os.scandir(base / "task") if entry.name.isdigit()} - set(tasks)
        if not fresh:
            return tasks
        for tid in fresh:
            try:
                tasks[tid] = task_view(base / "task" / tid)
            except (FileNotFoundError, ProcessLookupError):
                tasks[tid] = None
    raise MeasureError(f"session fence: threads kept appearing in process {base.name}")


def linux_maps(pid: int):
    """read_maps from every task: a leader that exited through pthread_exit has empty maps while its workers run."""
    base, uids = PROC_ROOT / str(pid), set()
    try:
        uids = task_view(base)[0]
        if int((base / "stat").read_bytes().rsplit(b")", 1)[1].split()[6]) & PF_KTHREAD:
            return uids, []
        tasks = linux_tasks(base)
    except (FileNotFoundError, ProcessLookupError):
        return None
    except OSError:
        tasks = {}
    live = [task for task in tasks.values() if task]
    uids = uids.union(*(task[0] for task in live))
    if bodies := [task[2] for task in live if task[2]]:
        # Any shared mapping counts: mprotect can make an already dirty page writable again without a new fault.
        return uids, [(os.makedev(*(int(v, 16) for v in f[3].split(b":"))), int(f[4]),
                       os.fsdecode(f[5].removesuffix(b" (deleted)")) if len(f) > 5 else "", f[1][3:4] == b"s")
                      for body in bodies for f in (line.split(maxsplit=5) for line in body.splitlines())]
    if any(task[2] is None for task in live):
        return uids, None
    if tasks and not live:
        return None
    return (uids, []) if live and all(task[1] in b"ZX" for task in live) else (uids, "undetermined")


def read_maps(pid: int):
    """(uids, [(dev, ino, path, may write)] or None if unreadable), or None for a process that is gone."""
    if sys.platform == "linux":
        for _ in range(3):
            found = linux_maps(pid)
            if found is None or found[1] != "undetermined":
                return found
            time.sleep(0.05)
        if 0 not in found[0]:
            raise MeasureError(f"session fence: cannot tell whether process {pid} still has an address space")
        return found[0], None
    lib, short, region, regions = libproc(), ShortInfo(), Region(), []
    if lib.proc_pidinfo(pid, 13, ctypes.c_uint64(0), ctypes.byref(short), ctypes.sizeof(short)) <= 0:
        if ctypes.get_errno() == errno.ESRCH:
            return None
        raise MeasureError(f"session fence: cannot identify process {pid}")
    while lib.proc_pidinfo(pid, 8, ctypes.c_uint64(region.address + region.size), ctypes.byref(region),
                           ctypes.sizeof(region)) > 0:
        regions.append((region.dev, region.ino, os.fsdecode(region.path), bool(region.max_prot & 2)))
    code = ctypes.get_errno()
    if not regions and code in (errno.ESRCH, errno.EPERM):
        return None if code == errno.ESRCH else ({short.uid, short.ruid, short.svuid}, None)
    return {short.uid, short.ruid, short.svuid}, regions


def describe(pid: int) -> str:
    """Name and gids of a Linux process whose maps were unreadable, next to this scanner's gids."""
    try:
        status = (PROC_ROOT / str(pid) / "status").read_text(errors="replace")
    except OSError:
        return ""
    name, gids = (re.search(rf"^{key}:\s*(.*)$", status, re.M) for key in ("Name", "Gid"))
    return (f"; name {name.group(1) if name else '?'}, gids {gids.group(1).split() if gids else '?'},"
            f" scanner gids {sorted({os.getgid(), os.getegid()})}")


def mapping_guard(stats: dict[str, list[int]], *, rounds: int = 10) -> dict:
    """Refuse a capture that a live process could still store into through a mapping; a store there may leave ctime."""
    if sys.platform not in ("linux", "darwin"):
        return dict(status="unavailable", platform=sys.platform)
    ids, me = {tuple(stat[:2]) for stat in stats.values()}, os.getuid()
    modes = [os.stat(name) for name in stats]
    owners, loose = {st.st_uid for st in modes}, any(st.st_mode & 0o022 for st in modes)
    seen, counts = set(), dict(processes=0, root=0, foreign=0)
    for _ in range(rounds):
        fresh = set(list_pids()) - seen
        if not fresh:
            break
        seen |= fresh
        for pid in sorted(fresh):
            found = read_maps(pid)
            if found is None:
                continue
            uids, regions = found
            if regions is None:
                if 0 not in uids and (me in uids or loose or uids & owners):
                    raise MeasureError(f"session fence: cannot read the mappings of process {pid} (uids {sorted(uids)}"
                                       f"{describe(pid)})")
                counts["root" if 0 in uids else "foreign"] += 1
                continue
            counts["processes"] += 1
            for dev, ino, path, writable in regions:
                if writable and ((dev, ino) in ids or path in stats):
                    raise MeasureError(f"session fence: process {pid} maps {path} shared and writable")
    else:
        raise MeasureError("session fence: processes kept appearing during the mapping scan")
    return dict(status="checked", platform=sys.platform, **counts)


def check_fence(fence, files_read: dict[str, bytes], root: Path) -> None:
    """The sealed stats cover exactly the captured files and their lookup directories, all older than the probe."""
    if not isinstance(fence, dict) or not isinstance(fence.get("files"), dict) or not fence["files"]:
        raise MeasureError("session fence: no stat list")
    check_probe(fence.get("probe"))
    files, dirs, probe, guard = fence["files"], fence.get("dirs"), fence["probe"], fence.get("mappings")
    if set(files) != set(files_read):
        raise MeasureError("session fence: stat list differs from the captured files")
    if not isinstance(dirs, dict) or set(dirs) != {str(d) for name in files for d in fence_dirs(root, name)}:
        raise MeasureError("session fence: directory list differs from the captured files' lookup path")
    if not isinstance(guard, dict) or guard.get("status") != "checked" or type(guard.get("processes")) is not int \
            or guard["processes"] < 1:
        raise MeasureError(f"session fence: shared-mapping guard not checked ({guard!r})")
    for name, stat in (*files.items(), *dirs.items()):
        width = len(modmap.STAT) if name in files else 4
        if not isinstance(stat, list) or len(stat) != width or any(type(v) is not int for v in stat):
            raise MeasureError(f"session fence: malformed stat for {name}")
        if name in files and stat[2] != len(files_read[name]):
            raise MeasureError(f"session fence: {name} stat size differs from its captured bytes")
        if stat[0] != probe["dev"]:
            raise MeasureError(f"session fence: {name} is not on the probed filesystem")
        if stat[-1] >= probe["ctimes"][-1]:
            raise MeasureError(f"session fence: {name} changed after the clock probe")


def fence_marker(fence: dict) -> dict:
    return dict(held=True, files=collect.digest(json.dumps(fence["files"], sort_keys=True).encode()),
                dirs=collect.digest(json.dumps(fence["dirs"], sort_keys=True).encode()),
                mappings=fence["mappings"]["status"], resolution_ns=check_probe(fence["probe"]))


DIR_STAT = ("dev", "ino", "mtime_ns", "ctime_ns")


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
    for name, stat in sorted(fence["dirs"].items()):
        try:
            now = dir_stat(name)
        except (OSError, MeasureError):
            now = None
        if now != stat:
            fields = "removed" if now is None else ",".join(f for f, a, b in zip(DIR_STAT, stat, now) if a != b)
            raise MeasureError(f"session fence: directory {name} changed during Cargo ({fields})")


def resolver(fail=MeasureError):
    """Path.resolve memoized per spelling, so a link swapped mid-check cannot reclassify a target."""
    resolved: dict[str, Path] = {}
    def canon(name: str) -> Path:
        if name not in resolved:
            resolved[name] = canonical(name, fail=fail)
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
    check_fence(request["fence"], files, Path(request["repo"]))
    data["request.json"] = request_bytes
    return dict(schema=collect.SCHEMA, kind="canary-items", manifest=str(run / "manifest.json"), run_dir=str(run),
                root=unit["root"], lane=request["lane"], run_id=request["run_id"], nonce=request["nonce"],
                request=request, proof=proof, fence=fence_marker(request["fence"]),
                digests={name: collect.digest(body) for name, body in data.items()})


def session(root: Path, crate: Path, run_dir: Path, conf_dir: Path, lane: str, *, extra=(),
            driver: Path | None = None, clippy: Path | None = None) -> dict:
    try:
        root, crate, conf_dir = (canonical(p, strict=True) for p in (root, crate, conf_dir))
        run = run_dir.absolute()
        if run != canonical(run) or (run.exists() and (not run.is_dir() or any(run.iterdir()))):
            raise MeasureError("session run directory must be new or empty and canonical")
        driver = canonical(driver or root / "target/modmap-driver/release/modmap-driver", strict=True)
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
        Path(crate, next((a.partition("=")[2] or b for a, b in zip(extra, extra[1:] + ("",))
                          if a.partition("=")[0] == "--target-dir"), unit["target_dir"])).mkdir(parents=True, exist_ok=True)
        os.utime(unit["lib"], None)
        probe = clock_probe(run / "fence.probe")
        print(f"h2-session: ctime fence probe {probe['writes']} writes, step {check_probe(probe)} ns, "
              f"{probe['same_tick']} same-tick rewrites", file=sys.stderr)
        state, files, stats = source_capture(root, Path(unit["lib"]), conf_dir)
        fence = dict(files=stats, dirs=seal_dirs(root, stats), probe=probe)
        fence["mappings"] = mapping_guard(stats)
        print(f"h2-session: shared-mapping guard {fence['mappings']}", file=sys.stderr)
        check_fence(fence, files, root)
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

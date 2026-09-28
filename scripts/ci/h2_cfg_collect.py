"""Validate and seal one module-map run; never invoke Cargo or compile a crate."""
from __future__ import annotations

import hashlib
import json
from pathlib import Path

import h2_cfg_compare
import h2_depinfo

SCHEMA = 1
MIN_MODULES = 1000


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def write_json(path: Path, value: dict) -> None:
    partial = path.with_name(path.name + ".partial")
    partial.write_text(json.dumps(value, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    partial.replace(path)


def read_json(path: Path) -> dict:
    value = json.loads(regular(path))
    if not isinstance(value, dict):
        raise ValueError(f"{path}: expected an object")
    return value


def regular(path: Path, start: int = 0) -> bytes:
    if path.absolute() != path.resolve() or not path.is_file():
        raise ValueError(f"{path}: expected a regular canonical run file")
    if path.stat().st_mtime_ns < start:
        raise ValueError(f"{path}: predates this run")
    return path.read_bytes()


def validate(run: Path) -> dict:
    regular(run / "start")
    request = read_json(run / "request.json")
    if type(request.get("schema")) is not int or request.get("schema") != SCHEMA or request.get("run_dir") != str(run.resolve()):
        raise ValueError("metadata: schema/run directory mismatch")
    kind, run_id, nonce = (request.get(key) for key in ("kind", "run_id", "nonce"))
    if kind not in ("root", "canary") or not all(isinstance(x, str) and len(x) == 32 for x in (run_id, nonce)):
        raise ValueError("metadata: invalid run identity")
    start = (run / "start").stat().st_mtime_ns
    paths = request.get("paths", {})
    if set(paths) != {"tsv", "cfg", "invocation", "cargo", "stdout", "stderr"}:
        raise ValueError("metadata: invalid artifact paths")
    cargo_path = Path(paths["cargo"])
    if cargo_path.parent != run:
        raise ValueError("metadata: artifact outside run directory")
    record = json.loads(regular(cargo_path, start))
    if not isinstance(record, dict) or type(record.get("rc")) is not int or record["rc"] != 0:
        raise ValueError("metadata: Cargo run failed")
    artifacts = {}
    for key, name in paths.items():
        path = Path(name)
        if path.parent != run or path.name in ("request.json", "start"):
            raise ValueError("metadata: artifact outside run directory")
        artifacts[key] = regular(path, start)
    if len(set(paths.values())) != len(paths):
        raise ValueError("metadata: aliased artifacts")
    invocation = json.loads(artifacts["invocation"])
    cfg = json.loads(artifacts["cfg"])
    for label, value in (("driver", invocation), ("cfg", cfg)):
        if (not isinstance(value, dict) or type(value.get("schema")) is not int or value.get("schema") != SCHEMA
                or value.get("run_id") != run_id or value.get("nonce") != nonce):
            raise ValueError(f"metadata: {label} run/nonce mismatch")
    manifest = Path(request["manifest"])
    if manifest.parent != run or str(manifest) in paths.values() or manifest.name.endswith(".partial"):
        raise ValueError("metadata: invalid final manifest path")
    root, driver = request["root"], request["driver"]
    command = record.get("argv", [])
    if (not isinstance(command, list) or command[:3] != ["cargo", "check", "--lib"]
            or "--message-format=json" not in command or "--manifest-path" not in command
            or command[command.index("--manifest-path") + 1:][:1] != [str(Path(root) / "Cargo.toml")]):
        raise ValueError("metadata: Cargo invocation mismatch")
    argv = invocation.get("argv")
    if (invocation.get("root") != root or invocation.get("kind") != kind
            or invocation.get("out") != paths["tsv"] or not isinstance(argv, list)
            or not all(isinstance(arg, str) for arg in argv) or len(argv) < 3
            or argv[0] != driver or "--test" in argv
            or not any((Path(root) / arg).resolve() == Path(root) / "src/lib.rs" for arg in argv[2:])):
        raise ValueError("metadata: driver argv/root/kind mismatch")
    if invocation.get("tsv") != artifacts["tsv"].decode("utf-8"):
        raise ValueError("metadata: driver TSV bytes mismatch")
    target = request["target"]
    for index, arg in enumerate(argv):
        actual = argv[index + 1] if arg == "--target" and index + 1 < len(argv) else arg.removeprefix("--target=")
        if (arg == "--target" or arg.startswith("--target=")) and actual != target:
            raise ValueError("metadata: driver target mismatch")
    if (request.get("lane") not in ("linux", "macos")
            or not isinstance(request.get("source"), dict) or not isinstance(request.get("toolchain"), dict)):
        raise ValueError("metadata: invalid source/toolchain/lane schema")
    if request.get("host") != target or f"host: {target}" not in request["toolchain"].get("rustc", "").splitlines():
        raise ValueError("metadata: host/target mismatch")
    atoms = h2_cfg_compare.read_cfg(Path(paths["cfg"]), data=artifacts["cfg"])
    if cfg.get("atoms") != [list(atom) for atom in sorted(atoms)]:
        raise ValueError("metadata: cfg must be sorted and unique")
    rows = h2_depinfo.load_modmap(Path(paths["tsv"]), data=artifacts["tsv"])
    files = sum(row.kind == "file" for row in rows)
    if files < (MIN_MODULES if kind == "root" else 1):
        raise ValueError(f"metadata: lists {files} file modules (< {MIN_MODULES if kind == 'root' else 1})")
    env = invocation.get("env")
    if not isinstance(env, dict) or env.get("MODMAP_RUN_ID") != run_id:
        raise ValueError("metadata: driver environment mismatch")
    events = [json.loads(line) for line in artifacts["stdout"].splitlines() if line.strip()]
    if not all(isinstance(event, dict) for event in events):
        raise ValueError("metadata: invalid Cargo JSONL")
    generated = {}
    for event in events:
        if event.get("reason") == "build-script-executed":
            directory = Path(event["out_dir"])
            generated[str(directory)] = {str(path.relative_to(directory)): digest(path.read_bytes())
                                         for path in sorted(directory.rglob("*")) if path.is_file()}
    return dict(schema=SCHEMA, kind=kind, run_id=run_id, nonce=nonce, run_dir=str(run), manifest=str(manifest),
                lane=request["lane"], root=root, host=request["host"], target=target, paths=paths,
                source=request["source"], toolchain=request["toolchain"], argv=argv, env=env,
                features=sorted(atom[1] for atom in atoms if atom[0] == "feature"), default_features=True,
                build_scripts=[event for event in events if event.get("reason") == "build-script-executed"],
                generated_inputs=generated, file_modules=files, cfg_digest=digest(artifacts["cfg"]), tsv_digest=digest(artifacts["tsv"]),
                digests={key: digest(data) for key, data in artifacts.items()},
                request_digest=digest(regular(run / "request.json", start)))


def seal(run: Path, manifest: Path) -> dict:
    if manifest.parent != run or manifest.exists() or manifest.is_symlink():
        raise ValueError("metadata: manifest must be new and inside run directory")
    value = validate(run)
    if value["manifest"] != str(manifest):
        raise ValueError("metadata: final manifest path mismatch")
    write_json(manifest, value)
    return value


def read_manifest(manifest: Path, *, lane: str, root: Path) -> dict:
    """A root bundle is consumable only through its final manifest, revalidated against its bytes."""
    value = read_json(manifest)
    if value.get("manifest") != str(manifest):
        raise ValueError("metadata: final manifest path mismatch")
    if value.get("kind") != "root" or value.get("lane") != lane or value.get("root") != str(root.resolve()):
        raise ValueError("metadata: root/lane/kind mismatch")
    if value != validate(manifest.parent):
        raise ValueError("metadata: manifest digest/content mismatch")
    return value

"""Map Clippy diagnostic sites to compiler item records of one sealed session; nothing calls this yet."""
from __future__ import annotations

import bisect
import json
import os
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path

import h2_cfg_collect as collect
import h2_modmap as modmap
import h2_session as session
from h2_measure import MeasureError

KIND = "canary-items"
SEALED = ("request.json", "session.json", "items.jsonl", "items.jsonl.sha256", "clippy.jsonl")
UNIT = ("manifest", "package", "lib", "crate_name", "crate_types")
LIB_KINDS = {"lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"}
IDENTITY = ("kind", "name", "crate_types", "src_path")
EXEC = {"fn", "nested_fn", "trait_method", "trait_impl_method", "inherent_method", "const"}
# Headers that own no executable code; their executable parts are separate records.
NON_EXEC = {"Struct", "Union", "Enum", "Use", "TyAlias", "TraitAlias", "Macro", "ExternCrate",
            "ForeignMod", "ForeignTy", "AssocTy", "OpaqueTy"}


class MappingError(MeasureError):
    def __init__(self, reason: str, detail: str = ""):
        super().__init__(f"{reason}: {detail}" if detail else reason)
        self.reason = reason


@dataclass
class Items:
    root: Path
    base: str | None
    rows: dict[str, list[dict]]
    raw: dict[str, bytes]
    breaks: dict[str, list[int]]
    messages: list[dict]
    foreign: int
    manifest: dict

    def line(self, name: str, offset: int) -> int:
        return bisect.bisect_left(self.breaks[name], offset) + 1


def load(path: Path, *, crate: Path) -> Items:
    """Accept only a sealed h2-session/2 manifest of the expected crate, parsing the digest-checked bytes."""
    try:
        value = collect.read_json(path)
        run = path.parent
        if value.get("manifest") != str(path) or value.get("run_dir") != str(run):
            raise MeasureError("items: manifest path mismatch")
        request, proof = value.get("request"), value.get("proof")
        if (type(value.get("schema")) is not int or value["schema"] != collect.SCHEMA or value.get("kind") != KIND
                or not isinstance(request, dict) or not isinstance(proof, dict) or request.get("kind") != KIND
                or request.get("schema") != session.SCHEMA or proof.get("schema") != session.SCHEMA):
            raise MeasureError("items: not an h2-session/2 items manifest")
        unit, claimed = request["unit"], proof.get("unit")
        root = Path(unit["manifest"]).parent
        if (unit["manifest"] != str((crate / "Cargo.toml").resolve(strict=True)) or not isinstance(claimed, dict)
                or any(claimed.get(key) != unit[key] for key in UNIT) or claimed.get("root") != str(root)
                or claimed.get("test") is not False or value.get("root") != str(root)):
            raise MeasureError("items: session unit differs from its request or the expected crate")
        for key in ("nonce", "run_id"):
            if not value.get(key) == request.get(key) == proof.get(key):
                raise MeasureError(f"items: session {key} mismatch")
        if list(run.glob("*.partial")):
            raise MeasureError("items: session has partial outputs")
        data = {name: collect.regular(run / name) for name in SEALED}
        digests = value.get("digests")
        if not isinstance(digests, dict) or any(digests.get(name) != collect.digest(body) for name, body in data.items()):
            raise MeasureError("items: session files differ from the sealed digests")
        if json.loads(data["request.json"]) != request or json.loads(data["session.json"]) != proof:
            raise MeasureError("items: manifest request/proof differ from the sealed files")
        session.validate_items(data, proof, request)
        records = modmap.item_records([json.loads(line) for line in data["items.jsonl"].splitlines()[1:]])
        events = [json.loads(line) for line in data["clippy.jsonl"].splitlines() if line.strip()]
        if not all(isinstance(event, dict) for event in events):
            raise MeasureError("items: invalid Cargo JSONL")
        messages, foreign = attribute(events, unit)
        repo = Path(request["repo"])
        state, files = session.source_capture(repo, Path(unit["lib"]), Path(request["conf_dir"]))
        if state != request["source"]:
            raise MeasureError("items: source changed after the session")
        rows: dict[str, list[dict]] = {}
        for record in records:
            rows.setdefault(str(root / record["file"]), []).append(record)
        raw = {name: files[name] for name in rows if name in files}
        items = Items(root, compiler_base(unit, proof), rows, raw,
                      {name: [m.start() for m in re.finditer(b"\n", body)] for name, body in raw.items()},
                      messages, foreign, value)
        for name, body in raw.items():
            for record in rows[name]:
                if record["hi"] > len(body) or items.line(name, record["lo"]) != record["line"]:
                    raise MappingError("coord", f"item {record['display']}: byte {record['lo']} is line "
                                       f"{items.line(name, record['lo'])} of {len(body)} original bytes, "
                                       f"compiler says {record['line']} (normalized offsets?)")
        return items
    except MappingError:
        raise
    except (MeasureError, OSError, ValueError, KeyError, TypeError, AttributeError, subprocess.SubprocessError) as exc:
        raise MappingError("unsealed", str(exc) if isinstance(exc, MeasureError) else f"items: {exc}") from exc


def attribute(events: list[dict], unit: dict) -> tuple[list[dict], int]:
    """Messages of the one proved lib compilation, and how many other units' messages were set aside."""
    resolved: dict[str, Path] = {}
    def canon(name: str) -> Path:  # each spelling is resolved once, so a swapped link cannot reclassify
        if name not in resolved:
            try:
                resolved[name] = Path(name).resolve()
            except RuntimeError as exc:
                raise MappingError("unsealed", f"items: cannot resolve {name}: {exc}") from exc
        return resolved[name]
    builds = [e for e in events if e.get("reason") == "compiler-artifact" and e.get("package_id") == unit["package_id"]
              and set(e["target"].get("kind") or ()) & LIB_KINDS and canon(e["target"]["src_path"]) == Path(unit["lib"])]
    if len(builds) != 1:
        raise MappingError("provenance", f"{len(builds)} compilations of the lib; its diagnostics are not attributable")
    sealed = builds[0]["target"]
    if (not set(sealed.get("kind") or ()) & LIB_KINDS or sealed.get("name", "").replace("-", "_") != unit["crate_name"]
            or sorted(sealed.get("crate_types") or ()) != unit["crate_types"]):
        raise MappingError("provenance", f"lib artifact target {sealed!r} is not the requested lib")
    own, foreign = [], 0
    for event in events:
        if event.get("reason") != "compiler-message":
            continue
        target = event.get("target")
        if (not all(isinstance(event.get(key), str) for key in ("package_id", "manifest_path"))
                or not isinstance(target, dict) or not isinstance(target.get("src_path"), str)
                or not isinstance(target.get("kind"), list) or not isinstance(event.get("message"), dict)):
            raise MappingError("provenance", "compiler message without its Cargo package/target")
        mine = event["package_id"] == unit["package_id"]
        if mine != (canon(event["manifest_path"]) == Path(unit["manifest"])):
            raise MappingError("provenance", f"package {event['package_id']} at {event['manifest_path']}")
        if mine and all(target.get(key) == sealed.get(key) for key in IDENTITY):
            own.append(event["message"])
        elif mine and set(target["kind"]) & LIB_KINDS:
            raise MappingError("provenance", f"message target {target!r} contradicts the lib artifact")
        else:
            foreign += 1
    return own, foreign


def compiler_base(unit: dict, proof: dict) -> str | None:
    """Cargo runs rustc in the workspace root; only a relative lib argument proves that base for file names."""
    root = unit["workspace_root"]
    if not isinstance(root, str) or not os.path.isabs(root):
        raise ValueError("request has no workspace root")
    spelled = [a for a in proof["argv"][1:]
               if not a.startswith("-") and os.path.normpath(os.path.join(root, a)) == unit["lib"]]
    return root if len(spelled) == 1 and not os.path.isabs(spelled[0]) else None


def primary(message: dict) -> dict:
    spans = message.get("spans", [])
    if not isinstance(spans, list) or not all(isinstance(span, dict) for span in spans):
        raise MappingError("coord", f"malformed diagnostic spans: {spans!r}")
    spans = [span for span in spans if span.get("is_primary") is True]
    if not spans:
        raise MappingError("no-item", f"diagnostic has no primary span: {message.get('message')!r}")
    return spans[0]


def site(span) -> dict:
    """The outermost macro call site, in original bytes as Clippy reports them."""
    while isinstance(span, dict) and span.get("expansion") is not None:
        expansion = span["expansion"]
        span = expansion.get("span") if isinstance(expansion, dict) else None
    if (not isinstance(span, dict) or not isinstance(span.get("file_name"), str)
            or any(type(span.get(key)) is not int or span[key] < 0 for key in ("byte_start", "byte_end", "line_start"))
            or span["byte_start"] > span["byte_end"]):
        raise MappingError("coord", f"malformed diagnostic span: {span!r}")
    return span


def resolve_tie(same: list[dict]) -> dict:
    """Keep every executable owner; drop only headers proven to be their container or non-executable."""
    if len(same) == 1:
        return same[0]
    execs = [r for r in same if r["kind"] in EXEC]
    parents = {r["parent"] for r in execs}
    unproven = [r for r in same if r["kind"] not in EXEC and r["def"] not in parents
                and re.match(r"\w*", r["def_kind"]).group() not in NON_EXEC]
    if unproven:
        raise MappingError("ambiguous:unproven-header:" + ",".join(sorted(r["display"] for r in unproven)))
    if len(execs) == 1:
        return execs[0]
    if not execs:
        return min(same, key=lambda r: r["def"])
    raise MappingError("ambiguous:" + ",".join(sorted(r["display"] for r in execs)))


def map_site(rows: list[dict], lo: int, hi: int) -> dict:
    hits = [r for r in rows if r["lo"] <= lo and hi <= r["hi"]]
    if not hits:
        raise MappingError("no-item", f"bytes {lo}..{hi}")
    width = min(r["hi"] - r["lo"] for r in hits)
    best = [r for r in hits if r["hi"] - r["lo"] == width]
    if len({(r["lo"], r["hi"]) for r in best}) > 1:
        raise MappingError("ambiguous:overlap")
    return resolve_tie(best)


def resolve(items: Items, span) -> tuple[str | None, str | None]:
    """(registration path, None) or (None, unregistrable reason); mapping failures raise MappingError."""
    where = site(span)
    if not os.path.isabs(where["file_name"]) and items.base is None:
        raise MappingError("no-item", f"{where['file_name']}: relative name without a proven compiler directory")
    name = os.path.normpath(os.path.join(items.base or "", where["file_name"]))
    lo, hi = where["byte_start"], where["byte_end"]
    if name in items.rows and name not in items.raw:
        raise MappingError("unsealed", f"{name} is outside the session source state")
    if name not in items.raw:
        raise MappingError("no-item", f"{where['file_name']}:{where['line_start']}")
    if hi > len(items.raw[name]) or items.line(name, lo) != where["line_start"]:
        raise MappingError("coord", f"diagnostic {where['file_name']}:{where['line_start']}: byte {lo} is line "
                           f"{items.line(name, lo)} of {len(items.raw[name])} original bytes")
    record = map_site(items.rows[name], lo, hi)
    return record["path"], record["unregistrable"]

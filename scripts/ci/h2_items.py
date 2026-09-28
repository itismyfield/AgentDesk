"""Map Clippy diagnostic sites to compiler item records of one sealed session; nothing calls this yet."""
from __future__ import annotations

import bisect
import json
import os
import re
from dataclasses import dataclass
from pathlib import Path

import h2_cfg_collect as collect
import h2_modmap as modmap
import h2_session as session
from h2_measure import MeasureError

KIND = "canary-items"
SEALED = ("request.json", "session.json", "items.jsonl", "items.jsonl.sha256", "clippy.jsonl")
UNIT = ("manifest", "package", "lib", "crate_name", "crate_types")
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
    rows: dict[str, list[dict]]
    raw: dict[str, bytes]
    breaks: dict[str, list[int]]
    messages: list[dict]
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
        messages = [event["message"] for event in events if event.get("reason") == "compiler-message"]
        if not all(isinstance(message, dict) for message in messages):
            raise MeasureError("items: invalid compiler message")
        repo = Path(request["repo"])
        listing = session.output(["git", "ls-files", "-c", "-o", "--exclude-standard", "-z"], repo, dict(os.environ))
        listed = {str(repo / name) for name in listing.split("\0") if name}
        rows: dict[str, list[dict]] = {}
        for record in records:
            rows.setdefault(str(root / record["file"]), []).append(record)
        # Only files covered by the session source state are read; the recheck below follows every read.
        raw = {name: Path(name).read_bytes() for name in rows if name in listed}
        items = Items(root, rows, raw, {name: [m.start() for m in re.finditer(b"\n", body)] for name, body in raw.items()},
                      messages, value)
        for name, body in raw.items():
            for record in rows[name]:
                if record["hi"] > len(body) or items.line(name, record["lo"]) != record["line"]:
                    raise MappingError("coord", f"item {record['display']}: byte {record['lo']} is line "
                                       f"{items.line(name, record['lo'])} of {len(body)} original bytes, "
                                       f"compiler says {record['line']} (normalized offsets?)")
        if session.source_state(repo, Path(unit["lib"]), Path(request["conf_dir"])) != request["source"]:
            raise MeasureError("items: source changed after the session")
        return items
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as exc:
        raise MeasureError(f"items: {exc}") from exc


def primary(message: dict) -> dict:
    spans = [span for span in message.get("spans") or () if isinstance(span, dict) and span.get("is_primary") is True]
    if not spans:
        raise MeasureError(f"items: diagnostic has no primary span: {message.get('message')!r}")
    return spans[0]


def site(span) -> dict:
    """The outermost macro call site, in original bytes as Clippy reports them."""
    while isinstance(span, dict) and span.get("expansion") is not None:
        expansion = span["expansion"]
        span = expansion.get("span") if isinstance(expansion, dict) else None
    if (not isinstance(span, dict) or not isinstance(span.get("file_name"), str)
            or any(type(span.get(key)) is not int or span[key] < 0 for key in ("byte_start", "byte_end", "line_start"))
            or span["byte_start"] > span["byte_end"]):
        raise MeasureError(f"items: malformed diagnostic span: {span!r}")
    return span


def resolve_tie(same: list[dict]) -> dict:
    """Keep every executable owner; drop only headers proven to be their container or non-executable."""
    if len(same) == 1:
        return same[0]
    execs = [r for r in same if r["kind"] in EXEC]
    parents = {r["parent"] for r in execs}
    unproven = [r for r in same if r["kind"] not in EXEC and r["def"] not in parents
                and re.match(r"\w+", r["def_kind"]).group() not in NON_EXEC]
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
    name = os.path.normpath(items.root / where["file_name"])
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

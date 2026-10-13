"""Strict manifest shape and explicit supported policy combinations."""
from datetime import datetime
import hashlib
from pathlib import Path
import re
from uuid import UUID

from . import Unknown, VERSION, loads

PACKAGE = Path(__file__).resolve().parent
FILES = ("process_proof", "executable", "transcript", "bindings", "baseline", "end_snapshot",
         "log", "o_init", "o_ledger", "legacy", "inflight", "pending", "abort", "debt", "pg", "discord")


def uint(value):
    return type(value) is int and value >= 0


def text(value):
    return isinstance(value, str) and bool(value.strip())


def sha(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None


def snowflake(value):
    return isinstance(value, str) and re.fullmatch(r"[1-9][0-9]*", value) is not None


SOURCE = {"session_id": text, "path": text, "dev": uint, "ino": uint}
IDENTITY = {"execution_nonce": text, "pane": text, "session": text}
RUNTIME = {"boot_id": text, "pid": uint, "build_commit": text,
           "executable_sha256": sha, "proof_kind": text}
SCHEMA = {
    "manifest_version": uint, "runtime": RUNTIME,
    "oracle": {"version": text, "bundle_sha256": sha},
    "policies": {"event_schema": uint, "native_identity": uint, "provider_format": text,
                 "seal": text, "exclusion": text, "split": text, "payload_digest": text, "ledger_schema": uint,
                 "normalization": text},
    "scope": {"channel_id": uint, "provider": text, "session_id": text, "source": SOURCE,
              "baseline_offset": uint, "fiftieth_offset": uint, "closure_offset": uint,
              "start_at": text, "end_at": text},
    "roots": {"runtime": text, "o_store": text},
    "observation": {"seq_start": uint, "seq_end": uint, "confirmation_seq": uint,
                    "actual_confirmed_at_us": lambda v: v is None or uint(v)},
    "binding": {"seq_start": uint, "seq_end": uint, "identity": IDENTITY},
    "source_digests": {"prefix_sha256": sha, "window_sha256": sha},
    "discord": {"channel_ids": [uint], "bot_author_ids": [uint], "lower_inclusive": snowflake,
                "upper_exclusive": snowflake, "individual_ids": [snowflake], "max_pages": uint},
    "files": {name: {"path": text, "sha256": sha} for name in FILES},
}


def shape(value, spec):
    if isinstance(spec, dict):
        return (isinstance(value, dict) and set(value) == set(spec)
                and all(shape(value[k], rule) for k, rule in spec.items()))
    if isinstance(spec, list):
        return isinstance(value, list) and all(shape(v, spec[0]) for v in value)
    return spec(value)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def bundle_digest():
    paths = [PACKAGE.parent / "n1_turn_canary_oracle.py", *sorted(PACKAGE.glob("*.py")),
             PACKAGE / "compatibility.json"]
    return digest(b"".join(p.name.encode() + b"\0" + p.read_bytes() + b"\0" for p in paths))


def producer_supported(runtime, table):
    spec = {"build_commit": lambda v: isinstance(v, str) and re.fullmatch(r"[0-9a-f]{40}", v) is not None,
            "executable_sha256": sha, "event_schema": uint,
            "unknown_outcome_produced": lambda v: type(v) is bool,
            "actual_confirmation_produced": lambda v: type(v) is bool}
    capabilities = table.get("producer_capabilities")
    if not shape(capabilities, [spec]):
        raise Unknown("producer_capabilities_invalid")
    return any(c["build_commit"] == runtime["build_commit"]
               and c["executable_sha256"] == runtime["executable_sha256"] and c["event_schema"] == 2
               and c["unknown_outcome_produced"] and c["actual_confirmation_produced"] for c in capabilities)


def validate(manifest):
    if not shape(manifest, SCHEMA):
        raise Unknown("manifest_invalid")
    table = loads((PACKAGE / "compatibility.json").read_bytes())
    scope, runtime, discord = manifest["scope"], manifest["runtime"], manifest["discord"]
    provider = scope["provider"]
    expected = {**table["policies"], "provider_format": table["provider_formats"].get(provider)}
    try:
        UUID(runtime["boot_id"])
        start = datetime.fromisoformat(scope["start_at"].replace("Z", "+00:00"))
        end = datetime.fromisoformat(scope["end_at"].replace("Z", "+00:00"))
        times_valid = start.tzinfo is not None and end.tzinfo is not None and start < end
    except ValueError:
        times_valid = False
    valid = (manifest["manifest_version"] == table["manifest_version"]
             and manifest["oracle"]["version"] == VERSION == table["oracle_version"]
             and manifest["oracle"]["bundle_sha256"] == bundle_digest()
             and manifest["policies"] == expected and times_valid
             and runtime["pid"] > 0 and re.fullmatch(r"[0-9a-f]{40}", runtime["build_commit"])
             and runtime["proof_kind"] == "proc-exe-build-attestation-v1"
             and scope["channel_id"] > 0 and scope["session_id"] == scope["source"]["session_id"]
             and scope["baseline_offset"] < scope["fiftieth_offset"] <= scope["closure_offset"]
             and discord["channel_ids"] == [scope["channel_id"]]
             and bool(discord["bot_author_ids"]) and all(x > 0 for x in discord["bot_author_ids"])
             and int(discord["lower_inclusive"]) < int(discord["upper_exclusive"])
             and 0 < discord["max_pages"] <= 10000
             and len(set(discord["individual_ids"])) == len(discord["individual_ids"]))
    for interval in (manifest["observation"], manifest["binding"]):
        valid = valid and 0 < interval["seq_start"] <= interval["seq_end"]
    if not valid:
        raise Unknown("manifest_invalid")
    return producer_supported(runtime, table)

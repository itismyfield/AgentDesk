"""Print an approval candidate from captured build identity and positive producer evidence."""
import argparse
import json
from pathlib import Path
import re
from uuid import UUID

from . import Unknown, loads
from .manifest import RUNTIME, SOURCE, digest, shape, text, uint


def candidate(proof, executable, release, rows):
    valid = (shape(proof, {**RUNTIME, "executable_path": text})
             and proof["pid"] > 0 and re.fullmatch(r"[0-9a-f]{40}", proof["build_commit"])
             and proof["proof_kind"] == "proc-exe-build-attestation-v1"
             and proof["executable_path"] == f"/proc/{proof['pid']}/exe"
             and digest(executable) == proof["executable_sha256"]
             and isinstance(release, dict) and release.get("repo_head") == proof["build_commit"]
             and release.get("repo_dirty") == "false")
    if not valid:
        raise Unknown("producer_identity_unverified")
    try:
        UUID(proof["boot_id"])
    except ValueError:
        raise Unknown("producer_identity_unverified") from None
    if (not rows or any(not isinstance(row, dict) or type(row.get("schema_version")) is not int
                       or row["schema_version"] != 2 or row.get("boot_id") != proof["boot_id"]
                       or not uint(row.get("channel_id")) or row["channel_id"] == 0
                       or row.get("provider") not in ("claude", "codex")
                       or not uint(row.get("seq")) or not uint(row.get("observed_at_us")) for row in rows)):
        raise Unknown("producer_capabilities_unverified")
    unknown, confirmed, sequences = set(), set(), {}
    logical_spec = {"provider": text, "channel_id": uint, "provider_session_id": text, "native_turn_id": text}
    for row in rows:
        sequence_scope = (row["boot_id"], row["channel_id"])
        if row["seq"] <= sequences.get(sequence_scope, -1):
            raise Unknown("producer_capabilities_unverified")
        sequences[sequence_scope] = row["seq"]
        key = (row["channel_id"], row["provider"])
        if row.get("event") == "n1_outcome_unknown":
            logical = row.get("logical_turn_key")
            if (shape(row.get("source_id"), SOURCE) and uint(row.get("witness_offset"))
                    and text(row.get("reason")) and "logical_turn_key" in row
                    and (logical is None or shape(logical, logical_spec)
                         and (logical["channel_id"], logical["provider"]) == key
                         and logical["provider_session_id"] == row["source_id"]["session_id"])):
                unknown.add(key)
        elif row.get("event") == "n1_mode_confirmed":
            before, actual = row.get("confirmation_window_start"), row.get("actual_confirmed_at_us")
            if (shape(before, {"seq": uint, "at_us": uint}) and uint(actual)
                    and before["seq"] < row["seq"] and before["at_us"] <= actual <= row["observed_at_us"]):
                confirmed.add(key)
    if not unknown & confirmed:
        raise Unknown("producer_capabilities_unverified")
    return dict(build_commit=proof["build_commit"], executable_sha256=proof["executable_sha256"],
                event_schema=2, unknown_outcome_produced=True, actual_confirmation_produced=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("process-proof", "executable", "release-source", "verification-log"):
        parser.add_argument("--" + name, required=True)
    args = parser.parse_args()
    try:
        raw = Path(args.verification_log).read_bytes()
        if not raw.endswith(b"\n"):
            raise Unknown("producer_capabilities_unverified")
        result = candidate(loads(Path(args.process_proof).read_bytes()), Path(args.executable).read_bytes(),
                           loads(Path(args.release_source).read_bytes()), [loads(line) for line in raw.splitlines()])
    except (OSError, ValueError, UnicodeError) as error:
        reason = str(error) if isinstance(error, Unknown) else "producer_evidence_invalid"
        print(json.dumps({"reason": reason}))
        return 2
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

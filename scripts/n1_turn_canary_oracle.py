#!/usr/bin/env python3
"""Independent N1 oracle input lane; final native and audit stages are pending."""
import argparse
import json
from pathlib import Path
import sys

from n1_canary import Unknown, loads
from n1_canary.discord import collect, getter, history_valid
from n1_canary.evidence import input_checks, observation_checks, pg_select, pg_valid, read_files
from n1_canary.manifest import digest, validate


class Parser(argparse.ArgumentParser):
    def exit(self, status=0, message=None):
        super().exit(2, message)


def main(argv=None):
    parser = Parser(description=__doc__)
    parser.add_argument("--manifest", required=True)
    for flag in ("baseline", "end-snapshot", "log", "runtime-root", "o-store-root",
                 "pg-evidence", "pg-dsn-env", "discord-history", "discord-base-url", "discord-token-env", "output"):
        parser.add_argument("--" + flag)
    args = parser.parse_args(argv)
    result = dict(schema_version=2, verdict="[U]", evidence_complete=False,
                  manifest_digest=None, observation={}, confirmation={}, evidence=[],
                  reasons=["stages_pending:o2,o3"])
    manifest_path = Path(args.manifest)
    manifest = None
    try:
        raw_manifest = manifest_path.read_bytes()
        candidate = loads(raw_manifest)
        validate(candidate)
        manifest = candidate
        result["manifest_digest"] = digest(raw_manifest)
        base = manifest_path.resolve().parent
        overrides = {name: getattr(args, name) for name in ("baseline", "end_snapshot", "log")}
        overrides.update(pg=args.pg_evidence, discord=args.discord_history)
        if (args.pg_evidence and args.pg_dsn_env) or (args.discord_history and args.discord_base_url):
            raise Unknown("input_settings_conflict")
        for name, given in (("runtime", args.runtime_root), ("o_store", args.o_store_root)):
            if given and Path(given).resolve() != (base / manifest["roots"][name]).resolve():
                raise Unknown("input_root_mismatch")
        omit = (["pg"] if args.pg_dsn_env else []) + (["discord"] if args.discord_base_url else [])
        values, raw, reasons, refs = read_files(manifest, base, overrides, omit)
        result["evidence"] = refs
        result["reasons"].extend(reasons + input_checks(manifest, values, raw))
        observation, confirmation, reasons = observation_checks(manifest, values)
        result.update(observation=observation, confirmation=confirmation)
        result["reasons"].extend(reasons)
        if args.pg_dsn_env:
            values["pg"] = pg_select(args.pg_dsn_env, manifest["scope"])
        if not pg_valid(values.get("pg")):
            result["reasons"].append("pg_invalid")
        if args.discord_base_url:
            values["discord"] = collect(manifest["discord"], getter(args.discord_base_url, args.discord_token_env))
        if not history_valid(values.get("discord"), manifest["discord"]):
            result["reasons"].append("discord_incomplete")
            if isinstance(values.get("discord"), dict):
                extra = values["discord"].get("reasons", [])
                if isinstance(extra, list):
                    result["reasons"].extend(x for x in extra if isinstance(x, str))
    except Unknown as error:
        result["reasons"].append(str(error))
    except (OSError, ValueError, UnicodeError):
        result["reasons"].append("manifest_invalid")
    result["reasons"] = sorted(set(result["reasons"]))
    if args.output:
        try:
            output = Path(args.output).resolve()
            if not manifest:
                raise Unknown("output_unavailable")
            if manifest:
                base = manifest_path.resolve().parent
                inputs = [manifest_path.resolve(), *(Path(p).resolve() for p in overrides.values() if p)]
                inputs.extend((base / f["path"]).resolve() for f in manifest["files"].values())
                roots = [(base / p).resolve() for p in manifest["roots"].values()]
                if output in inputs or any(output.is_relative_to(root) for root in roots):
                    raise Unknown("output_unavailable")
            with output.open("x", encoding="utf-8") as stream:
                stream.write(json.dumps(result, sort_keys=True) + "\n")
        except (OSError, Unknown):
            result["reasons"].append("output_unavailable")
    print(json.dumps(result, sort_keys=True))
    return 2


if __name__ == "__main__":
    sys.exit(main())

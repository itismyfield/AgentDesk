"""agy headless E2E cell: enqueue, drain, ensure, preflight and run (E-65)."""
import argparse
import json
from pathlib import Path
import sys
import time

from scripts.dev_instance.driver_admission import IdentityRefused, admit
from scripts.dev_instance.files import InstanceError, safe_absolute

from . import instance, scenario


def preflight(base, instance_id, spec):
    """Read-only: Driver admission and an idle lease; never sends."""
    try:
        admitted = admit(base, instance_id, time.monotonic() + spec["admission_s"])
        channel = admitted.manifest["profile"]["channels"][0]
        lease = scenario.local_json(f"{admitted.manifest['api']['self_url']}/api/turn-lease/antigravity/{channel}",
                                    time.monotonic() + spec["preflight_s"])
    except (IdentityRefused, InstanceError, KeyError, IndexError):
        return {"status": "UNEVALUABLE(precondition)", "transmissions": 0}
    return {"status": "PRECHECK_OK" if lease is None else "UNEVALUABLE(precondition)", "transmissions": 0}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    queue = commands.add_parser("enqueue")
    queue.add_argument("--config", required=True)
    queue.add_argument("--request-id", required=True)
    queue.add_argument("--head", required=True)
    commands.add_parser("drain").add_argument("--config", required=True)
    prepare = commands.add_parser("ensure")
    prepare.add_argument("--config", required=True)
    prepare.add_argument("--head", required=True)
    for name in ("preflight", "run"):
        sub = commands.add_parser(name)
        sub.add_argument("--instance", required=True)
        if name == "run":
            sub.add_argument("--scenario", choices=("E-65",), default="E-65")
    args = parser.parse_args(argv)
    base = safe_absolute(Path.home() / ".adk/dev/instances")
    spec = scenario.load_spec()
    try:
        if args.command == "enqueue":
            result = instance.enqueue(args.config, args.request_id, args.head)
        elif args.command == "preflight":
            result = preflight(base, args.instance, spec)
        elif args.command == "run":
            result = scenario.execute(base, args.instance, spec)
        else:
            config = instance.load_config(args.config)
            if args.command == "ensure":
                result = {"status": "READY", "instance_id": instance.ensure(config, args.head)}
            else:
                def run_request(request):
                    try:
                        instance_id = instance.ensure(config, request["deploy_sha"])
                    except (InstanceError, OSError, ValueError, KeyError):
                        return {"status": "INSTANCE_UNAVAILABLE", "transmissions": 0}
                    return instance.live_run(config, instance_id, request["deploy_sha"],
                                             lambda uuid: scenario.execute(base, uuid, spec))
                result = instance.drain(config, run_request)
    except (InstanceError, OSError, ValueError) as error:
        result = {"status": "REJECTED", "reason": type(error).__name__}
    print(json.dumps(result, sort_keys=True, ensure_ascii=False))
    return 0 if result.get("status") in ("PASS", "PRECHECK_OK", "READY", "SUBMITTED", "DISABLED", "IDLE",
                                          "DEFERRED_DEPLOY", "DEFERRED_BUSY") else 3


if __name__ == "__main__":
    sys.exit(main())

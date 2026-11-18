"""CLI: `tick` (launchd), `hold -- argv` (manual E2E under the channel lock), `submit` (deploy inbox)."""

import argparse
import json
from pathlib import Path
import sys
import time

from . import channel_lock, inbox, runner


def main(argv=None):
    parser = argparse.ArgumentParser(prog="python3 -m scripts.e2e.continuous")
    parser.add_argument("--config", required=True)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("tick")
    hold = commands.add_parser("hold")
    hold.add_argument("argv", nargs=argparse.REMAINDER)
    submit = commands.add_parser("submit")
    submit.add_argument("--request-id", required=True)
    submit.add_argument("--deploy-sha", required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "tick":
            result = runner.tick(args.config)
        elif args.command == "hold":
            command = args.argv[1:] if args.argv[:1] == ["--"] else args.argv
            if not command:
                parser.error("hold needs a command after --")
            return runner.hold(args.config, command)
        else:
            state = runner.load_config(args.config).get("state_dir")
            if not isinstance(state, str) or not Path(state).is_absolute():
                raise ValueError("state_dir must be an absolute path")
            result = {"status": "SUBMITTED",
                      **inbox.submit(Path(state), args.request_id, args.deploy_sha, time.time())}
    except channel_lock.LeaseBusy:
        result = {"status": "DEFERRED_LEASE"}
    except (channel_lock.LeaseError, ValueError, OSError) as error:
        result = {"status": "REJECTED", "reason": str(error)[:200]}
    print(json.dumps(result, sort_keys=True))
    return {"REJECTED": 2, "DEFERRED_LEASE": 5}.get(result["status"], 0)


if __name__ == "__main__":
    sys.exit(main())

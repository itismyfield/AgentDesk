"""I1 declarations only. Lifecycle commands are supplied by later stages."""

import argparse
import json
import os
import sys
import uuid
from pathlib import Path

from .files import BlockedBinary, InstanceError, instance_root, safe_absolute, write_credential
from .manifest import child_environment, prepare, read_json, validate
from .profile import Unevaluable, require_preconditions


class Parser(argparse.ArgumentParser):
    def error(self, message):
        raise InstanceError("invalid command arguments; inline credentials are forbidden")


def main(argv=None):
    parser = Parser(description="Declare isolated test instances; does not launch services")
    commands = parser.add_subparsers(dest="command", required=True, parser_class=Parser)
    create = commands.add_parser("prepare")
    create.add_argument("worktree")
    create.add_argument("--profile", required=True)
    create.add_argument("--instance", default=None)
    create.add_argument("--api-port", type=int)
    create.add_argument("--pg-port", type=int)
    inspect = commands.add_parser("validate")
    inspect.add_argument("--instance", required=True)
    inspect.add_argument("--require-credentials", action="store_true")
    inspect.add_argument("--witness")
    credential = commands.add_parser("credential", help="Read one root credential from stdin")
    credential.add_argument("--instance", required=True)
    credential.add_argument("--name", required=True)
    try:
        args = parser.parse_args(argv)
        base = safe_absolute(Path.home() / ".adk/dev/instances")
        if args.command == "prepare":
            profile = read_json(Path(args.profile).absolute())
            instance_id = args.instance or str(uuid.uuid4())
            result = prepare(base, instance_id, Path(args.worktree).absolute(), profile, os.environ,
                             args.api_port, args.pg_port)
        else:
            result = validate(base, args.instance, os.environ,
                              require_credentials=getattr(args, "require_credentials", False))
            if args.command == "credential":
                write_credential(instance_root(base, args.instance), args.name, sys.stdin.buffer)
                result = {"status": "CREDENTIAL_WRITTEN", "instance_id": args.instance}
            elif args.witness:
                require_preconditions(result, read_json(Path(args.witness).absolute()))
                result = {"status": "PRECONDITIONS_CONFIRMED", "live_ready": False}
            else:
                child_environment(result, os.environ)
                result = {"status": "DECLARATION_VALID", "live_ready": False, "instance_id": args.instance}
        print(json.dumps(result, sort_keys=True))
        return 0
    except BlockedBinary:
        print(json.dumps({"status": "BLOCKED_BINARY"}))
        return 4
    except Unevaluable:
        print(json.dumps({"status": "UNEVALUABLE(precondition)"}))
        return 3
    except (InstanceError, OSError, ValueError, TypeError):
        print(json.dumps({"status": "REJECTED", "reason": "invalid isolated instance declaration"}))
        return 2


if __name__ == "__main__":
    sys.exit(main())

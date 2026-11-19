"""Submit a secret-free deploy companion request without changing the deployment outcome."""
import argparse
import json
from pathlib import Path
import time

from . import inbox
from .channel_lock import LeaseError


def submit(state_dir, request_id, deploy_sha, *, now=None):
    try:
        if not state_dir or not Path(state_dir).is_absolute():
            raise ValueError("state_dir must be an absolute path")
        row = inbox.submit(Path(state_dir), request_id, deploy_sha, time.time() if now is None else now)
        return {"status": "SUBMITTED", **row}
    except (LeaseError, OSError, ValueError, TypeError):
        return {"status": "REJECTED", "reason": "companion request unavailable or invalid"}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-dir")
    parser.add_argument("--request-id")
    parser.add_argument("--deploy-sha")
    try:
        args = parser.parse_args(argv)
    except SystemExit:
        return 0
    print(json.dumps(submit(args.state_dir, args.request_id, args.deploy_sha), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

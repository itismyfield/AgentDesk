"""Read-only briefing/monitor CLI for local status files or a secret-free atomic mirror."""
import argparse
import os
from pathlib import Path
import time

from . import summary
from .store import read_json


def _read(path):
    try:
        missing = object()
        value = read_json(path, missing)
        return None if value is missing else {"schema_version": 0} if value is None else value
    except OSError:
        return {"schema_version": 0, "unavailable": True}
    except ValueError:
        return {"schema_version": 0}


def view(status_file, *, head, now, summary_file=None):
    value = _read(status_file) if status_file else None
    if isinstance(value, dict) and "status" in value:
        if type(value.get("schema_version")) is int and value["schema_version"] == 1:
            state, report = value.get("status"), value.get("summary")
        else:
            state, report = {"schema_version": 0}, None
    else:
        state = value
        report = _read(summary_file or Path(status_file).with_name("summary.json")) if status_file else None
    return summary.consume(report, state, head=head, now=now)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--status-file", default=os.environ.get("BC_E2E_STATUS_FILE"))
    parser.add_argument("--summary-file")
    parser.add_argument("--head", default=os.environ.get("BC_E2E_HEAD", ""))
    args = parser.parse_args(argv)
    for line in summary.format_lines(view(args.status_file, head=args.head, now=time.time(), summary_file=args.summary_file)):
        print(line)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

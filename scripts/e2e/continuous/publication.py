"""Supervisor publication: durable run -> summary/outbox -> status; consumers only read."""
from contextlib import contextmanager
import threading
import time

from . import outbox, plan, summary
from .store import read_json, write_json


def status(state, config, now, running=None):
    last = read_json(state / "summary.json", {}) if config.get("enabled") is True else {}
    cursor = plan.cursor(state) if config.get("enabled") is True else {}
    interval = config.get("rotation_interval_s")
    due = cursor.get("last_scheduled_at")
    return {"schema_version": 1, "enabled": config.get("enabled") is True, "heartbeat_at": now,
            # Consumers expire results by the active period: the rotation, else the core re-run.
            "tick_interval_s": config.get("tick_interval_s", 60),
            "rotation_interval_s": interval if interval is not None else config.get("core_interval_s"),
            "max_run_s": config.get("max_run_s", 600), "tick_grace_s": config.get("tick_grace_s", 60),
            "running": running, "last_completed_run": last.get("run_id"),
            "outbox_pending": outbox.pending(state) if config.get("enabled") is True else 0,
            "next_due": due + interval if due is not None and interval is not None else None}


def heartbeat(state, config, now, running=None):
    value = status(state, config, now, running)
    write_json(state / "status.json", value)
    return value


@contextmanager
def keepalive(state, config, run_id):
    """Keep a blocking driver's supervisor observable; stop the writer before final publication."""
    stop, errors = threading.Event(), []

    def pulse():
        while not stop.wait(config.get("tick_interval_s", 60)):
            try:
                heartbeat(state, config, time.time(), run_id)
            except OSError as error:
                errors.append(error)
                return

    thread = threading.Thread(target=pulse, name="e2e-heartbeat")
    thread.start()
    try:
        yield
    finally:
        stop.set()
        thread.join()
    if errors:
        raise errors[0]


def finish(state, record, config):
    value = summary.build(record, config)
    now = record.get("completed_at", time.time())
    outbox.enqueue(state, value, now)
    value["notification_pending"] = outbox.pending(state)
    write_json(state / "summaries" / f"{record['run_id']}.json", value)
    previous = read_json(state / "summary.json", {})
    if not previous or previous.get("completed_at", 0) <= value.get("completed_at", 0):
        write_json(state / "summary.json", value)
    heartbeat(state, config, now)


def view(config, *, head, now):
    """Off is an explicit live supervisor observation, with no state reads or writes."""
    if config.get("enabled") is not True:
        return summary.consume(None, {"schema_version": 1, "enabled": False, "heartbeat_at": now}, head=head, now=now)
    state = config["state_dir"]

    def read(path):
        try:
            return read_json(path, None)
        except (OSError, ValueError):
            return {"schema_version": 0}

    return summary.consume(read(state / "summary.json"), read(state / "status.json"), head=head, now=now)

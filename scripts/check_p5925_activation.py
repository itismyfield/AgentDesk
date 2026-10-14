#!/usr/bin/env python3
"""Read-only check of the operator survey that gates enabling the #5925 legacy restart replay.

usage: check_p5925_activation.py <survey.json>
Prints P5925_ACTIVATION_OK (rc 0) only when every writer of the declared roots was stopped and
restarted on the approved source SHA, each root resolved, and every recovery source carries a
source-authority policy verdict and its proof inputs. Any missing or contradicting item prints
P5925_ACTIVATION_REFUSED with one REFUSE line per reason (rc 1). Unreadable input is rc 2.

The pinned source policy is decided by the Rust source authority; this script only checks that
the survey records that verdict and never derives one from environment variables.
"""
import json
import re
import sys

SHA1 = re.compile(r"^[0-9a-f]{40}$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
WRITER_ROLES = {"primary", "standby", "worker", "repair", "wrapper", "normalizer", "supervisor"}
POLICIES = {"Legacy", "VerifiedCurrent", "VerifiedUnavailable"}


def _items(survey, key, refuse):
    value = survey.get(key)
    records = [item for item in value if isinstance(item, dict)] if isinstance(value, list) else []
    if isinstance(value, list) and len(records) != len(value):
        refuse(f"{key}: malformed record in survey section")
    if not records:
        refuse(f"{key}: survey section is missing or empty")
    return records


def _text(value):
    return isinstance(value, str) and bool(value.strip())


def _scope(survey, key, refuse):
    value = survey.get(key)
    if not isinstance(value, list) or not value or not all(_text(item) for item in value):
        refuse(f"{key}: declared scope is missing or malformed")
        return set()
    if len(set(value)) != len(value):
        refuse(f"{key}: duplicate scope entry")
    return set(value)


def _writer_identity(writer, label, hosts, roots, refuse):
    if not _text(writer.get("host")) or writer["host"] not in hosts:
        refuse(f"{label}: host outside the declared scope")
    if not _text(writer.get("role")) or writer["role"] not in WRITER_ROLES:
        refuse(f"{label}: role unknown")
    if any(type(writer.get(key)) is not int or writer[key] <= 0 for key in ("pid", "pgid")):
        refuse(f"{label}: positive PID/PGID not recorded")
    if not _text(writer.get("executable_realpath")) or not _text(writer.get("started_at")):
        refuse(f"{label}: executable realpath or start time not recorded")
    if not _text(writer.get("root_realpath")):
        refuse(f"{label}: root identity not recorded")
    elif writer["root_realpath"] not in roots:
        refuse(f"{label}: runs against an unresolved root")


def _coverage(label, record, key, identity, phases, refuse):
    present = [any(writer.get(key) == identity for writer in writers) for writers in phases]
    evidence = record.get("writer_free_evidence")
    if _text(evidence):
        if any(present):
            refuse(f"{label}: writer-free evidence contradicts writer records")
    elif not all(present):
        refuse(f"{label}: writer coverage missing for stop or restart without writer-free evidence")
    elif "writer_free_evidence" in record:
        refuse(f"{label}: writer-free evidence is malformed")


def check(survey):
    reasons = []
    refuse = reasons.append
    approved = survey.get("approved_source_sha")
    if not _text(approved) or not SHA1.fullmatch(approved):
        refuse("approved_source_sha: not a full 40-hex source SHA")
    artifacts = _items(survey, "artifacts", refuse)
    approved_hashes = set()
    for artifact in artifacts:
        if not _text(artifact.get("path")):
            refuse("artifact: path not recorded")
        if artifact.get("source_sha") != approved:
            refuse(f"artifact {artifact.get('path')}: built from {artifact.get('source_sha')}, not the approved SHA")
        elif not _text(artifact.get("sha256")) or not SHA256.fullmatch(artifact["sha256"]):
            refuse(f"artifact {artifact.get('path')}: sha256 missing")
        else:
            approved_hashes.add(artifact["sha256"])

    declared_hosts = _scope(survey, "declared_hosts", refuse)
    declared_roots = _scope(survey, "declared_roots", refuse)
    if not declared_hosts or not declared_roots:
        refuse("declared_hosts/declared_roots: the writer set has no declared scope")
    hosts = {}
    for host in _items(survey, "hosts", refuse):
        name = host.get("host")
        if not _text(name) or name not in declared_hosts:
            refuse(f"host {name}: outside the declared scope")
        elif name in hosts:
            refuse(f"host {name}: duplicate host survey")
        else:
            hosts[name] = host
    surveyed = {name for name, host in hosts.items() if host.get("surveyed") is True}
    for host in sorted(declared_hosts - surveyed):
        refuse(f"host {host}: declared but not surveyed")

    resolved = {}
    roots = {}
    for root in _items(survey, "roots", refuse):
        name = root.get("declared")
        if not _text(name) or name not in declared_roots:
            refuse(f"root {name}: outside the declared scope")
            continue
        if name in roots:
            refuse(f"root {name}: duplicate root survey")
            continue
        roots[name] = root
        if root.get("accessible") is not True or not _text(root.get("realpath")) or not _text(root.get("fs_id")):
            refuse(f"root {name}: realpath, filesystem id or access not confirmed")
            continue
        resolved[name] = (root["realpath"], root["fs_id"])
    for name in sorted(declared_roots - set(resolved)):
        refuse(f"root {name}: declared but not resolved")
    by_target = {}
    for name, target in resolved.items():
        by_target.setdefault(target, []).append(name)
    for names in by_target.values():
        if len(names) > 1:
            refuse(f"roots {sorted(names)}: different declared roots resolve to one root")

    stopped = _items(survey, "writers_stopped", refuse)
    restarted = _items(survey, "writers_restarted", refuse)
    root_paths = {target[0] for target in resolved.values()}
    for writer in stopped:
        label = f"stopped writer {writer.get('host')}:{writer.get('pid')}"
        _writer_identity(writer, label, declared_hosts, root_paths, refuse)
        if not _text(writer.get("supervisor_label")) and writer.get("supervisor_confirmed_none") is not True:
            refuse(f"{label}: auto-restart supervisor not confirmed")
        if writer.get("alive") is not False or writer.get("restartable") is not False:
            refuse(f"{label}: still alive or restartable after the stop")

    for writer in restarted:
        label = f"restarted writer {writer.get('host')}:{writer.get('pid')}"
        _writer_identity(writer, label, declared_hosts, root_paths, refuse)
        if not _text(writer.get("executable_sha256")) or writer["executable_sha256"] not in approved_hashes:
            refuse(f"{label}: running executable is not an approved artifact")
    for name, host in hosts.items():
        _coverage(f"host {name}", host, "host", name, (stopped, restarted), refuse)
    for name, target in resolved.items():
        _coverage(f"root {name}", roots[name], "root_realpath", target[0], (stopped, restarted), refuse)

    for source in _items(survey, "sources", refuse):
        label = f"source {source.get('channel')}/{source.get('tmux')}"
        if not _text(source.get("channel")) or not _text(source.get("tmux")):
            refuse(f"{label}: channel or tmux not recorded")
        policy = source.get("policy")
        if not _text(policy) or policy not in POLICIES:
            refuse(f"{label}: pinned policy unknown")
        elif policy == "VerifiedUnavailable":
            refuse(f"{label}: VerifiedUnavailable is never downgraded to Legacy")
        if source.get("policy_evidence") != "source_authority" or not _text(source.get("incarnation")):
            refuse(f"{label}: policy not taken from the tmux source authority with its incarnation")
        identity = source.get("raw_file_identity")
        if (
            not _text(source.get("session_id")) or not _text(source.get("raw_path"))
            or not isinstance(identity, list) or len(identity) != 2
            or any(type(value) is not int or value < 0 for value in identity)
        ):
            refuse(f"{label}: session or opened raw source unconfirmed")
        if source.get("recovery_target") is True and (
            policy != "Legacy"
            or not all(_text(source.get(key)) for key in ("native_turn_id", "nonce", "body_msg_id"))
            or source.get("proof") != "planned"
        ):
            refuse(f"{label}: selected for recovery without Legacy policy, identity and a planned proof")
    return reasons


def main(argv):
    if len(argv) != 2:
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 2
    try:
        with open(argv[1], encoding="utf-8") as handle:
            survey = json.load(handle)
    except (OSError, ValueError) as error:
        print(f"P5925_ACTIVATION_ERROR unreadable survey: {error}", file=sys.stderr)
        return 2
    if not isinstance(survey, dict):
        print("P5925_ACTIVATION_ERROR survey must be a JSON object", file=sys.stderr)
        return 2
    reasons = check(survey)
    if reasons:
        print("P5925_ACTIVATION_REFUSED")
        for reason in reasons:
            print(f"REFUSE {reason}")
        return 1
    print("P5925_ACTIVATION_OK")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

"""Executable, side-effect-free W1-A contract for AgentDesk issue #4254.

This module deliberately imports no AgentDesk runtime code.  It models the
authority, recovery-ledger, bounded-disposition, and source-to-Discord oracle
rules that later production wiring must preserve.
"""

from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass, replace
from enum import Enum
from typing import Any, Iterable, Mapping, Sequence


SCHEMA_VERSION = 1
MAX_U64 = (1 << 64) - 1
DEFAULT_MAX_FUTURE_SKEW_MS = 300_000
MARKER_RE = re.compile(
    r"^\[ADK-E2E:(?P<run>[^:\]]+):(?P<case>[^:\]]+):"
    r"(?P<sequence>[0-9]+):(?P<edge>BEGIN|END)\]$"
)
MARKER_SCAN_RE = re.compile(
    r"\[ADK-E2E:[^:\]\r\n]+:[^:\]\r\n]+:[0-9]+:(?:BEGIN|END)\]"
)


class Verdict(str, Enum):
    PRODUCER_LIVE = "producer_live"
    CONTROL_PLANE_DESYNC = "control_plane_desync"
    PRODUCER_DEAD = "producer_dead"
    DELIVERED_IDLE = "delivered_idle"


class AllowedAction(str, Enum):
    NONE = "none"
    OBSERVE = "observe"
    REPAIR_CONTROL_PLANE = "repair_control_plane"
    DELEGATE_EXACT_TERMINATION = "delegate_exact_termination"


class InflightContinuity(str, Enum):
    PRESENT_STABLE = "present_stable"
    MISSING = "missing"
    REAPPEARED_SAME = "reappeared_same"
    REPLACED = "replaced"


class QueueDisposition(str, Enum):
    ACTIVE = "active"
    IDLE = "idle"
    TERMINAL = "terminal"
    RETRYABLE = "retryable"
    UNKNOWN = "unknown"


@dataclass(frozen=True)
class ClockStamp:
    wall_ms: int
    boot_id: str
    monotonic_ms: int


@dataclass(frozen=True)
class DeliveryProof:
    episode_key: str
    source_id: str
    source_generation: int
    durable_anchor: str
    source_frontier: int
    committed_frontier: int
    queue: QueueDisposition
    observed_at: ClockStamp
    identity_complete: bool = True
    clock_valid: bool = True


@dataclass(frozen=True)
class TerminationProof:
    episode_key: str
    source_id: str
    source_generation: int
    durable_anchor: str
    queue: QueueDisposition
    termination_id: str
    finalizer_committed: bool
    watcher_stopped: bool
    action_reprobe_at: ClockStamp
    identity_complete: bool = True
    clock_valid: bool = True


@dataclass(frozen=True)
class Observation:
    episode_key: str | None
    source_id: str | None = None
    source_generation: int | None = None
    durable_anchor: str | None = None
    observed_at: ClockStamp | None = None
    identity_complete: bool = True
    clock_valid: bool = True
    continuity: InflightContinuity = InflightContinuity.PRESENT_STABLE
    queue: QueueDisposition = QueueDisposition.ACTIVE
    termination_proof: TerminationProof | None = None
    # Compatibility-only input. A boolean can never prove ProducerDead.
    affirmative_termination: bool = False
    source_progress: bool = False
    delivery_progress: bool = False
    control_contradiction: bool = False
    terminal_delivery_committed: bool = False
    typed_idle: bool = False
    failed_repairs: int = 0


@dataclass(frozen=True)
class Assessment:
    candidate: Verdict | None
    action: AllowedAction
    reason: str
    may_spend_automatic_action: bool


def assess(observation: Observation) -> Assessment:
    """Return the fail-closed authoritative assessment for one observation."""

    if not isinstance(observation.continuity, InflightContinuity):
        return _inconclusive("continuity_not_typed")
    if not isinstance(observation.queue, QueueDisposition):
        return _inconclusive("queue_not_typed")
    if not observation.identity_complete or not observation.episode_key:
        return _inconclusive("missing_identity")
    if not observation.clock_valid:
        return _inconclusive("invalid_clock")
    if observation.continuity is InflightContinuity.MISSING:
        return _inconclusive("inflight_missing_4104")
    if observation.continuity is InflightContinuity.REAPPEARED_SAME:
        return _inconclusive("inflight_reappeared_stability_window_4104")
    if observation.continuity is InflightContinuity.REPLACED:
        return _inconclusive("episode_replaced")
    if observation.queue in {QueueDisposition.RETRYABLE, QueueDisposition.UNKNOWN}:
        return _inconclusive("queue_not_authoritative_4247")

    if observation.terminal_delivery_committed and observation.typed_idle:
        return Assessment(
            Verdict.DELIVERED_IDLE,
            AllowedAction.OBSERVE,
            "terminal_delivery_and_typed_idle",
            False,
        )

    if observation.termination_proof is not None:
        proof = observation.termination_proof
        proof_matches = (
            isinstance(proof.queue, QueueDisposition)
            and isinstance(observation.queue, QueueDisposition)
            and proof.identity_complete
            and proof.clock_valid
            and isinstance(proof.episode_key, str)
            and bool(proof.episode_key)
            and isinstance(proof.source_id, str)
            and bool(proof.source_id)
            and isinstance(proof.source_generation, int)
            and not isinstance(proof.source_generation, bool)
            and 0 <= proof.source_generation <= MAX_U64
            and isinstance(proof.durable_anchor, str)
            and bool(proof.durable_anchor)
            and proof.episode_key == observation.episode_key
            and proof.source_id == observation.source_id
            and proof.source_generation == observation.source_generation
            and proof.durable_anchor == observation.durable_anchor
            and proof.queue is observation.queue
            and proof.action_reprobe_at == observation.observed_at
            and _clock_stamp_well_formed(proof.action_reprobe_at)
            and isinstance(proof.termination_id, str)
            and bool(proof.termination_id)
            and proof.finalizer_committed
            and proof.watcher_stopped
        )
        if not proof_matches:
            return _inconclusive("termination_proof_identity_or_reprobe_mismatch")
        if proof.queue in {QueueDisposition.TERMINAL, QueueDisposition.IDLE}:
            return Assessment(
                Verdict.PRODUCER_DEAD,
                AllowedAction.DELEGATE_EXACT_TERMINATION,
                "episode_bound_termination_and_safe_queue_disposition",
                False,
            )
        return _inconclusive("termination_proof_but_queue_still_active")

    if observation.control_contradiction and (
        observation.source_progress or observation.delivery_progress
    ):
        return Assessment(
            Verdict.CONTROL_PLANE_DESYNC,
            AllowedAction.REPAIR_CONTROL_PLANE,
            "live_progress_with_control_contradiction",
            True,
        )
    if observation.source_progress or observation.delivery_progress:
        return Assessment(
            Verdict.PRODUCER_LIVE,
            AllowedAction.OBSERVE,
            "affirmative_progress",
            False,
        )

    # Silence, age, and failed repairs are deliberately not death proof.
    if observation.failed_repairs:
        return _inconclusive("failed_repairs_are_not_death_proof")
    return _inconclusive("no_affirmative_evidence")


def _inconclusive(reason: str) -> Assessment:
    return Assessment(None, AllowedAction.NONE, reason, False)


class LedgerState(str, Enum):
    RESERVED = "reserved"
    EFFECT_PENDING = "effect_pending"
    SETTLED = "settled"


class LedgerOutcome(str, Enum):
    VERIFIED_DELIVERY_PROGRESS = "verified_delivery_progress"
    VERIFIED_CONTROL_REPAIR = "verified_control_repair"
    NO_PROGRESS = "no_progress"
    SUPERSEDED = "superseded"
    INCONCLUSIVE = "inconclusive"
    FAILED = "failed"


def clock_allows_transition(
    previous: ClockStamp,
    current: ClockStamp,
    *,
    trusted_now: ClockStamp | None = None,
    max_future_skew_ms: int = DEFAULT_MAX_FUTURE_SKEW_MS,
) -> bool:
    """Validate time only as an eligibility guard, never as liveness proof."""

    values = (
        previous.wall_ms,
        previous.monotonic_ms,
        current.wall_ms,
        current.monotonic_ms,
    )
    if (
        any(
            isinstance(value, bool)
            or not isinstance(value, int)
            or value < 0
            or value > MAX_U64
            for value in values
        )
        or not isinstance(previous.boot_id, str)
        or not previous.boot_id
        or not isinstance(current.boot_id, str)
        or not current.boot_id
        or isinstance(max_future_skew_ms, bool)
        or not isinstance(max_future_skew_ms, int)
        or max_future_skew_ms < 0
        or max_future_skew_ms > MAX_U64
    ):
        return False
    if trusted_now is not None:
        trusted_values = (trusted_now.wall_ms, trusted_now.monotonic_ms)
        if (
            any(
                isinstance(value, bool)
                or not isinstance(value, int)
                or value < 0
                or value > MAX_U64
                for value in trusted_values
            )
            or not isinstance(trusted_now.boot_id, str)
            or not trusted_now.boot_id
            or trusted_now.wall_ms > MAX_U64 - max_future_skew_ms
            or trusted_now.monotonic_ms > MAX_U64 - max_future_skew_ms
            or previous.wall_ms > trusted_now.wall_ms + max_future_skew_ms
            or current.wall_ms > trusted_now.wall_ms + max_future_skew_ms
            or (
                previous.boot_id == trusted_now.boot_id
                and previous.monotonic_ms
                > trusted_now.monotonic_ms + max_future_skew_ms
            )
            or (
                current.boot_id == trusted_now.boot_id
                and current.monotonic_ms
                > trusted_now.monotonic_ms + max_future_skew_ms
            )
        ):
            return False
    if current.wall_ms < previous.wall_ms:
        return False
    if previous.boot_id == current.boot_id:
        return current.monotonic_ms >= previous.monotonic_ms
    # Across boots, monotonic values are incomparable; nondecreasing wall time
    # is only a conservative eligibility guard and never liveness evidence.
    return True


def _clock_stamp_well_formed(stamp: ClockStamp | None) -> bool:
    if stamp is None or not isinstance(stamp.boot_id, str) or not stamp.boot_id:
        return False
    return all(
        not isinstance(value, bool)
        and isinstance(value, int)
        and 0 <= value <= MAX_U64
        for value in (stamp.wall_ms, stamp.monotonic_ms)
    )


@dataclass(frozen=True)
class LedgerRecord:
    schema_version: int
    attempt_id: str
    episode_key: str
    source_id: str
    source_generation: int
    durable_anchor: str
    baseline_source: int
    baseline_delivered: int
    state: LedgerState
    outcome: LedgerOutcome
    failure_streak: int
    lifetime_no_effect_count: int
    attempts_spent: int
    retry_not_before_ms: int | None
    clock_stamp: ClockStamp

    @property
    def circuit_open(self) -> bool:
        return self.lifetime_no_effect_count >= 2


@dataclass(frozen=True)
class LedgerTransition:
    record: LedgerRecord | None
    changed: bool
    spent_delta: int
    reason: str
    must_reprobe: bool = False


def _delivery_proof_error(
    proof: DeliveryProof | None,
    observation: Observation,
    *,
    now: ClockStamp,
    record: LedgerRecord | None = None,
) -> str | None:
    if proof is None:
        return "delivery_proof_required"
    if (
        not isinstance(proof.queue, QueueDisposition)
        or not isinstance(observation.queue, QueueDisposition)
        or not isinstance(proof.episode_key, str)
        or not proof.episode_key
        or not isinstance(proof.source_id, str)
        or not proof.source_id
        or not isinstance(proof.durable_anchor, str)
        or not proof.durable_anchor
        or isinstance(proof.source_generation, bool)
        or not isinstance(proof.source_generation, int)
        or isinstance(proof.source_frontier, bool)
        or not isinstance(proof.source_frontier, int)
        or isinstance(proof.committed_frontier, bool)
        or not isinstance(proof.committed_frontier, int)
        or not _clock_stamp_well_formed(proof.observed_at)
    ):
        return "delivery_proof_malformed"
    if (
        not proof.identity_complete
        or not proof.clock_valid
        or not observation.identity_complete
        or not observation.clock_valid
    ):
        return "delivery_proof_identity_or_clock_invalid"
    if (
        proof.episode_key != observation.episode_key
        or proof.source_id != observation.source_id
        or proof.source_generation != observation.source_generation
        or proof.durable_anchor != observation.durable_anchor
        or proof.queue is not observation.queue
    ):
        return "delivery_proof_observation_mismatch"
    if proof.queue in {QueueDisposition.RETRYABLE, QueueDisposition.UNKNOWN}:
        return "delivery_proof_queue_inconclusive"
    if proof.observed_at != now:
        return "delivery_proof_not_action_time"
    if observation.observed_at != now or not _clock_stamp_well_formed(
        observation.observed_at
    ):
        return "delivery_observation_not_action_time"
    integers = (
        proof.source_generation,
        proof.source_frontier,
        proof.committed_frontier,
    )
    if (
        any(value < 0 or value > MAX_U64 for value in integers)
        or proof.committed_frontier > proof.source_frontier
    ):
        return "delivery_proof_malformed"
    if record is not None:
        if (
            proof.episode_key != record.episode_key
            or proof.source_id != record.source_id
            or proof.source_generation != record.source_generation
        ):
            return "delivery_proof_record_identity_mismatch"
        if proof.source_frontier < record.baseline_source:
            return "delivery_source_frontier_regressed"
        if proof.committed_frontier < record.baseline_delivered:
            return "delivery_frontier_regressed"
        advanced = proof.committed_frontier > record.baseline_delivered
        if advanced and proof.durable_anchor == record.durable_anchor:
            return "delivery_anchor_did_not_advance"
        if advanced != observation.delivery_progress:
            return "delivery_progress_claim_mismatch"
    return None


def reserve_attempt(
    record: LedgerRecord | None,
    observation: Observation,
    *,
    attempt_id: str,
    delivery_proof: DeliveryProof,
    now: ClockStamp,
) -> LedgerTransition:
    if (
        record is not None
        and record.state in {LedgerState.RESERVED, LedgerState.EFFECT_PENDING}
    ):
        return LedgerTransition(
            record,
            False,
            0,
            "attempt_unresolved_requires_exact_reprobe",
            True,
        )
    assessment = assess(observation)
    if not assessment.may_spend_automatic_action:
        return LedgerTransition(record, False, 0, assessment.reason)
    if not isinstance(attempt_id, str) or not attempt_id:
        return LedgerTransition(record, False, 0, "attempt_id_missing")

    proof_error = _delivery_proof_error(delivery_proof, observation, now=now)
    if proof_error is not None:
        return LedgerTransition(record, False, 0, proof_error, True)

    if record is not None:
        if not clock_allows_transition(record.clock_stamp, now):
            return LedgerTransition(record, False, 0, "clock_fail_closed")
        if record.episode_key == observation.episode_key:
            if attempt_id == record.attempt_id:
                return LedgerTransition(record, False, 0, "attempt_id_reused")
            proof_error = _delivery_proof_error(
                delivery_proof, observation, now=now, record=record
            )
            if proof_error is not None:
                return LedgerTransition(record, False, 0, proof_error, True)
            if delivery_proof.committed_frontier > record.baseline_delivered:
                record = replace(
                    record,
                    durable_anchor=delivery_proof.durable_anchor,
                    baseline_delivered=delivery_proof.committed_frontier,
                    baseline_source=delivery_proof.source_frontier,
                    failure_streak=0,
                    lifetime_no_effect_count=0,
                    retry_not_before_ms=None,
                    outcome=LedgerOutcome.VERIFIED_DELIVERY_PROGRESS,
                    clock_stamp=now,
                )
            elif record.circuit_open:
                # Source-only progress is intentionally ignored here.
                return LedgerTransition(record, False, 0, "same_episode_circuit_open")
            if (
                record.retry_not_before_ms is not None
                and now.wall_ms < record.retry_not_before_ms
            ):
                return LedgerTransition(record, False, 0, "backoff_active")
        else:
            record = None

    failure_streak = 0 if record is None else record.failure_streak
    lifetime_no_effect_count = 0 if record is None else record.lifetime_no_effect_count
    attempts_spent = 0 if record is None else record.attempts_spent
    reserved = LedgerRecord(
        schema_version=SCHEMA_VERSION,
        attempt_id=attempt_id,
        episode_key=observation.episode_key or "",
        source_id=delivery_proof.source_id,
        source_generation=delivery_proof.source_generation,
        durable_anchor=delivery_proof.durable_anchor,
        baseline_source=delivery_proof.source_frontier,
        baseline_delivered=delivery_proof.committed_frontier,
        state=LedgerState.RESERVED,
        outcome=LedgerOutcome.INCONCLUSIVE,
        failure_streak=failure_streak,
        lifetime_no_effect_count=lifetime_no_effect_count,
        attempts_spent=attempts_spent,
        retry_not_before_ms=None,
        clock_stamp=now,
    )
    return LedgerTransition(reserved, True, 0, "reserved_without_spend")


def mark_effect_pending(
    record: LedgerRecord, *, attempt_id: str, now: ClockStamp
) -> LedgerTransition:
    if attempt_id != record.attempt_id:
        return LedgerTransition(record, False, 0, "attempt_cas_mismatch", True)
    if record.state is not LedgerState.RESERVED:
        return LedgerTransition(record, False, 0, "not_reserved")
    if not clock_allows_transition(record.clock_stamp, now):
        return LedgerTransition(record, False, 0, "clock_fail_closed")
    if record.attempts_spent >= MAX_U64:
        return LedgerTransition(record, False, 0, "attempt_counter_overflow", True)
    pending = replace(
        record,
        state=LedgerState.EFFECT_PENDING,
        attempts_spent=record.attempts_spent + 1,
        clock_stamp=now,
    )
    return LedgerTransition(pending, True, 1, "effect_boundary_crossed")


def recover_after_crash(
    record: LedgerRecord,
    observation: Observation | None = None,
    *,
    attempt_id: str | None = None,
) -> LedgerTransition:
    if attempt_id != record.attempt_id:
        return LedgerTransition(record, False, 0, "attempt_cas_mismatch", True)
    if record.state is LedgerState.RESERVED:
        if (
            observation is None
            or observation.episode_key != record.episode_key
            or observation.source_id != record.source_id
            or observation.source_generation != record.source_generation
            or observation.durable_anchor != record.durable_anchor
            or observation.continuity is not InflightContinuity.PRESENT_STABLE
            or not observation.identity_complete
            or not observation.clock_valid
        ):
            return LedgerTransition(record, False, 0, "reserved_requires_reprobe", True)
        cancelled = replace(
            record,
            state=LedgerState.SETTLED,
            outcome=LedgerOutcome.INCONCLUSIVE,
        )
        return LedgerTransition(cancelled, True, 0, "reserved_action_not_invoked")
    if record.state is LedgerState.EFFECT_PENDING:
        return LedgerTransition(record, False, 0, "pending_requires_reprobe", True)
    return LedgerTransition(record, False, 0, "already_settled")


def settle_effect(
    record: LedgerRecord,
    observation: Observation,
    *,
    attempt_id: str,
    delivery_proof: DeliveryProof | None,
    now: ClockStamp,
    backoff_ms: int = 600_000,
) -> LedgerTransition:
    if attempt_id != record.attempt_id:
        return LedgerTransition(record, False, 0, "attempt_cas_mismatch", True)
    if record.state is not LedgerState.EFFECT_PENDING:
        return LedgerTransition(record, False, 0, "not_effect_pending")
    if not isinstance(observation.continuity, InflightContinuity):
        return LedgerTransition(record, False, 0, "continuity_not_typed", True)
    if observation.continuity in {
        InflightContinuity.MISSING,
        InflightContinuity.REAPPEARED_SAME,
    }:
        return LedgerTransition(record, False, 0, "inflight_continuity_inconclusive", True)
    if not clock_allows_transition(record.clock_stamp, now):
        return LedgerTransition(record, False, 0, "clock_fail_closed", True)
    if observation.episode_key != record.episode_key or (
        observation.continuity is InflightContinuity.REPLACED
    ):
        successor_identity_valid = (
            observation.identity_complete
            and observation.clock_valid
            and isinstance(observation.continuity, InflightContinuity)
            and isinstance(observation.queue, QueueDisposition)
            and observation.queue
            not in {QueueDisposition.RETRYABLE, QueueDisposition.UNKNOWN}
            and isinstance(observation.episode_key, str)
            and bool(observation.episode_key)
            and isinstance(observation.source_id, str)
            and bool(observation.source_id)
            and isinstance(observation.source_generation, int)
            and not isinstance(observation.source_generation, bool)
            and 0 <= observation.source_generation <= MAX_U64
            and isinstance(observation.durable_anchor, str)
            and bool(observation.durable_anchor)
            and observation.observed_at == now
            and _clock_stamp_well_formed(observation.observed_at)
        )
        if not successor_identity_valid:
            return LedgerTransition(record, False, 0, "successor_identity_inconclusive", True)
        settled = replace(
            record,
            state=LedgerState.SETTLED,
            outcome=LedgerOutcome.SUPERSEDED,
            clock_stamp=now,
        )
        return LedgerTransition(settled, True, 0, "successor_untouched")
    proof_error = _delivery_proof_error(
        delivery_proof, observation, now=now, record=record
    )
    if proof_error is not None:
        return LedgerTransition(record, False, 0, proof_error, True)
    assert delivery_proof is not None
    if delivery_proof.committed_frontier > record.baseline_delivered:
        settled = replace(
            record,
            state=LedgerState.SETTLED,
            outcome=LedgerOutcome.VERIFIED_DELIVERY_PROGRESS,
            durable_anchor=delivery_proof.durable_anchor,
            baseline_source=delivery_proof.source_frontier,
            baseline_delivered=delivery_proof.committed_frontier,
            failure_streak=0,
            lifetime_no_effect_count=0,
            retry_not_before_ms=None,
            clock_stamp=now,
        )
        return LedgerTransition(settled, True, 0, "delivery_progress_rearms")

    # Source growth without delivery is still a no-effect repair.
    if (
        isinstance(backoff_ms, bool)
        or not isinstance(backoff_ms, int)
        or backoff_ms < 0
        or backoff_ms > MAX_U64
        or now.wall_ms > MAX_U64 - backoff_ms
        or record.failure_streak >= MAX_U64
        or record.lifetime_no_effect_count >= MAX_U64
    ):
        return LedgerTransition(record, False, 0, "backoff_clock_overflow", True)
    settled = replace(
        record,
        state=LedgerState.SETTLED,
        outcome=LedgerOutcome.NO_PROGRESS,
        baseline_source=delivery_proof.source_frontier,
        failure_streak=record.failure_streak + 1,
        lifetime_no_effect_count=record.lifetime_no_effect_count + 1,
        retry_not_before_ms=now.wall_ms + backoff_ms,
        clock_stamp=now,
    )
    return LedgerTransition(settled, True, 0, "no_delivery_progress")


@dataclass(frozen=True)
class LedgerLoad:
    record: LedgerRecord | None
    fail_closed: bool
    reason: str


def load_ledger_json(
    raw: str,
    *,
    trusted_now: ClockStamp,
    max_future_skew_ms: int = DEFAULT_MAX_FUTURE_SKEW_MS,
) -> LedgerLoad:
    try:
        data = json.loads(raw)
    except (TypeError, json.JSONDecodeError):
        return LedgerLoad(None, True, "corrupt_json")
    if not isinstance(data, dict):
        return LedgerLoad(None, True, "record_not_object")
    version = data.get("schema_version")
    if isinstance(version, bool) or (version is not None and not isinstance(version, int)):
        return LedgerLoad(None, True, "malformed_record")
    if version is None or version == 0:
        return LedgerLoad(None, True, "legacy_inconclusive")
    if version != SCHEMA_VERSION:
        return LedgerLoad(None, True, "future_or_unknown_schema")
    try:
        clock = data["clock_stamp"]
        record = LedgerRecord(
            schema_version=version,
            attempt_id=_required_text(data, "attempt_id"),
            episode_key=_required_text(data, "episode_key"),
            source_id=_required_text(data, "source_id"),
            source_generation=_nonnegative_int(data, "source_generation"),
            durable_anchor=_required_text(data, "durable_anchor"),
            baseline_source=_nonnegative_int(data, "baseline_source"),
            baseline_delivered=_nonnegative_int(data, "baseline_delivered"),
            state=LedgerState(data["state"]),
            outcome=LedgerOutcome(data["outcome"]),
            failure_streak=_nonnegative_int(data, "failure_streak"),
            lifetime_no_effect_count=_nonnegative_int(data, "lifetime_no_effect_count"),
            attempts_spent=_nonnegative_int(data, "attempts_spent"),
            retry_not_before_ms=(
                None
                if data.get("retry_not_before_ms") is None
                else _nonnegative_int(data, "retry_not_before_ms")
            ),
            clock_stamp=ClockStamp(
                wall_ms=_nonnegative_int(clock, "wall_ms"),
                boot_id=_required_text(clock, "boot_id"),
                monotonic_ms=_nonnegative_int(clock, "monotonic_ms"),
            ),
        )
    except (KeyError, TypeError, ValueError):
        return LedgerLoad(None, True, "malformed_record")
    if not clock_allows_transition(
        record.clock_stamp,
        trusted_now,
        trusted_now=trusted_now,
        max_future_skew_ms=max_future_skew_ms,
    ):
        return LedgerLoad(None, True, "clock_fail_closed")
    if not _ledger_semantics_valid(record):
        return LedgerLoad(None, True, "semantic_invariant_violation")
    return LedgerLoad(record, False, "valid")


def _required_text(data: Mapping[str, Any], key: str) -> str:
    value = data[key]
    if not isinstance(value, str) or not value:
        raise ValueError(key)
    return value


def _nonnegative_int(data: Mapping[str, Any], key: str) -> int:
    value = data[key]
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or value < 0
        or value > MAX_U64
    ):
        raise ValueError(key)
    return value


def _ledger_semantics_valid(record: LedgerRecord) -> bool:
    if record.baseline_delivered > record.baseline_source:
        return False
    if record.failure_streak > record.lifetime_no_effect_count:
        return False
    if record.lifetime_no_effect_count > record.attempts_spent:
        return False
    if record.state is LedgerState.RESERVED:
        return (
            record.outcome is LedgerOutcome.INCONCLUSIVE
            and record.retry_not_before_ms is None
        )
    if record.state is LedgerState.EFFECT_PENDING:
        return (
            record.outcome is LedgerOutcome.INCONCLUSIVE
            # mark_effect_pending spends one action before any corresponding
            # no-effect settlement can be counted.
            and record.attempts_spent > record.lifetime_no_effect_count
            and record.retry_not_before_ms is None
        )
    if record.outcome is LedgerOutcome.INCONCLUSIVE:
        # The only settled inconclusive transition cancels an uninvoked
        # reservation, so it cannot retain a retry deadline.
        return record.retry_not_before_ms is None
    if record.outcome is LedgerOutcome.NO_PROGRESS:
        return (
            record.attempts_spent >= 1
            and record.failure_streak >= 1
            and record.lifetime_no_effect_count >= 1
            and record.retry_not_before_ms is not None
            and record.retry_not_before_ms >= record.clock_stamp.wall_ms
        )
    if record.outcome in {
        LedgerOutcome.VERIFIED_DELIVERY_PROGRESS,
        LedgerOutcome.VERIFIED_CONTROL_REPAIR,
    }:
        return (
            record.attempts_spent >= 1
            and record.failure_streak == 0
            and record.lifetime_no_effect_count == 0
            and record.retry_not_before_ms is None
        )
    if record.outcome is LedgerOutcome.SUPERSEDED:
        return (
            record.attempts_spent > record.lifetime_no_effect_count
            and record.retry_not_before_ms is None
        )
    if record.outcome is LedgerOutcome.FAILED:
        # A terminal action failure is a no-effect settlement and must carry
        # the same spent-attempt, failure-count, and retry evidence.
        return (
            record.attempts_spent >= 1
            and record.failure_streak >= 1
            and record.lifetime_no_effect_count >= 1
            and record.retry_not_before_ms is not None
            and record.retry_not_before_ms >= record.clock_stamp.wall_ms
        )
    return False


class RangeKind(str, Enum):
    ASSISTANT_TEXT = "assistant_text"
    TOOL_BULK = "tool_bulk"
    CONTROL = "control"


class DispositionKind(str, Enum):
    DELIVERED = "delivered"
    SUMMARIZED = "summarized"
    OMITTED_POLICY = "omitted_policy"


@dataclass(frozen=True)
class SourceRange:
    range_id: str
    start: int
    end: int
    kind: RangeKind
    body: str
    marker: str | None = None

    @property
    def raw(self) -> bytes:
        return self.body.encode("utf-8")


@dataclass(frozen=True)
class PlannedDisposition:
    range_ids: tuple[str, ...]
    start: int
    end: int
    kind: DispositionKind
    source_sha256: str
    source_range_sha256s: tuple[str, ...]
    expected_body: str
    policy_reason: str
    committed: bool = False
    discord_message_ids: tuple[str, ...] = ()


@dataclass(frozen=True)
class BatchPlan:
    dispositions: tuple[PlannedDisposition, ...]
    processed_end: int
    admitted_items: int
    admitted_bytes: int
    blocked_reason: str | None


def plan_bounded_batch(
    ranges: Sequence[SourceRange],
    *,
    start_cursor: int,
    max_pending_items: int,
    max_pending_bytes: int,
    policy_version: str = "relay-target-v1",
) -> BatchPlan:
    bounds = (start_cursor, max_pending_items, max_pending_bytes)
    if any(
        isinstance(value, bool)
        or not isinstance(value, int)
        or value < 0
        or value > MAX_U64
        for value in bounds
    ):
        raise ValueError("u64 bounds required")
    if max_pending_items == 0 or max_pending_bytes == 0:
        raise ValueError("positive bounds required")
    if (
        not isinstance(policy_version, str)
        or re.fullmatch(r"[A-Za-z0-9._-]+", policy_version) is None
    ):
        raise ValueError("policy version required")
    _validate_source_ranges(ranges, start_cursor)
    dispositions: list[PlannedDisposition] = []
    used_items = 0
    used_bytes = 0
    cursor = start_cursor
    index = 0
    blocked: str | None = None

    while index < len(ranges):
        item = ranges[index]
        if item.kind is RangeKind.ASSISTANT_TEXT:
            size = len(item.raw)
            if size > max_pending_bytes:
                blocked = "high_value_range_requires_utf8_safe_split"
                break
            if used_items + 1 > max_pending_items or used_bytes + size > max_pending_bytes:
                blocked = "bounded_window_full"
                break
            dispositions.append(
                PlannedDisposition(
                    (item.range_id,),
                    item.start,
                    item.end,
                    DispositionKind.DELIVERED,
                    _hash_ranges([item]),
                    (_hash_ranges([item]),),
                    item.body,
                    "assistant_text_must_deliver",
                )
            )
            used_items += 1
            used_bytes += size
            cursor = item.end
            index += 1
            continue

        if item.kind is RangeKind.CONTROL:
            if used_items + 1 > max_pending_items:
                blocked = "bounded_window_full"
                break
            dispositions.append(
                PlannedDisposition(
                    (item.range_id,),
                    item.start,
                    item.end,
                    DispositionKind.OMITTED_POLICY,
                    _hash_ranges([item]),
                    (_hash_ranges([item]),),
                    "",
                    f"{policy_version}:control",
                )
            )
            used_items += 1
            cursor = item.end
            index += 1
            continue

        group_start = index
        raw_size = 0
        while index < len(ranges) and ranges[index].kind is RangeKind.TOOL_BULK:
            raw_size += len(ranges[index].raw)
            index += 1
        group_end = index
        group_count = group_end - group_start
        raw_items_fit = (
            used_items + group_count <= max_pending_items
            and used_bytes + raw_size <= max_pending_bytes
        )
        if raw_items_fit:
            for entry in ranges[group_start:group_end]:
                dispositions.append(
                    PlannedDisposition(
                        (entry.range_id,),
                        entry.start,
                        entry.end,
                        DispositionKind.DELIVERED,
                        _hash_ranges([entry]),
                        (_hash_ranges([entry]),),
                        entry.body,
                        f"{policy_version}:tool_within_bound",
                    )
                )
                used_items += 1
                used_bytes += len(entry.raw)
                cursor = entry.end
            continue

        if used_items + 1 > max_pending_items:
            index = group_start
            blocked = "bounded_window_full"
            break
        available_bytes = max_pending_bytes - used_bytes
        source_scan_budget = max_pending_bytes * max_pending_items
        prefix: list[SourceRange] = []
        prefix_raw_bytes = 0
        summary = ""
        for candidate in ranges[group_start:group_end]:
            candidate_raw_bytes = prefix_raw_bytes + len(candidate.raw)
            if candidate_raw_bytes > source_scan_budget and prefix:
                break
            candidate_prefix = [*prefix, candidate]
            candidate_summary = _tool_summary(
                candidate_prefix,
                policy_version,
                max_bytes=available_bytes,
            )
            if len(candidate_summary.encode("utf-8")) > available_bytes:
                break
            prefix = candidate_prefix
            prefix_raw_bytes = candidate_raw_bytes
            summary = candidate_summary
        if not prefix:
            index = group_start
            blocked = "tool_summary_exceeds_window"
            break
        summary_size = len(summary.encode("utf-8"))
        dispositions.append(
            PlannedDisposition(
                tuple(entry.range_id for entry in prefix),
                prefix[0].start,
                prefix[-1].end,
                DispositionKind.SUMMARIZED,
                _hash_ranges(prefix),
                tuple(_hash_ranges([entry]) for entry in prefix),
                summary,
                f"{policy_version}:tool_bulk_overflow",
            )
        )
        used_items += 1
        used_bytes += summary_size
        cursor = prefix[-1].end
        index = group_start + len(prefix)
        if index < group_end:
            blocked = "bounded_tool_bulk_suffix"
            break

    return BatchPlan(tuple(dispositions), cursor, used_items, used_bytes, blocked)


def commit_dispositions(
    plan: BatchPlan,
    discord_ids: Mapping[int, Sequence[str]],
) -> tuple[PlannedDisposition, ...]:
    if not isinstance(discord_ids, Mapping):
        raise ValueError("Discord ids must be an index mapping")
    for index in discord_ids:
        if (
            isinstance(index, bool)
            or not isinstance(index, int)
            or index < 0
            or index >= len(plan.dispositions)
        ):
            raise ValueError("Discord id mapping index is invalid")
    committed: list[PlannedDisposition] = []
    for index, disposition in enumerate(plan.dispositions):
        if not isinstance(disposition.kind, DispositionKind):
            raise ValueError("unknown disposition kind")
        raw_ids = discord_ids.get(index, ())
        if (
            not isinstance(raw_ids, Sequence)
            or isinstance(raw_ids, (str, bytes, bytearray))
        ):
            raise ValueError("Discord ids must be a sequence")
        ids = tuple(raw_ids)
        if any(not _discord_message_id_valid(value) for value in ids):
            raise ValueError("Discord message id is invalid")
        if disposition.kind in {DispositionKind.DELIVERED, DispositionKind.SUMMARIZED}:
            if len(ids) != 1 or any(not value for value in ids):
                raise ValueError(
                    "exactly one Discord id required for delivered/summary disposition"
                )
        elif ids:
            raise ValueError("policy omission cannot claim a Discord message")
        committed.append(
            replace(disposition, committed=True, discord_message_ids=ids)
        )
    return tuple(committed)


def serialize_planned_disposition(disposition: PlannedDisposition) -> dict[str, Any]:
    """Serialize the planner contract in the exact shape consumed by the oracle."""

    if not isinstance(disposition.kind, DispositionKind):
        raise ValueError("unknown disposition kind")
    return {
        "range_ids": list(disposition.range_ids),
        "kind": disposition.kind.value,
        "policy_reason": disposition.policy_reason,
        "source_sha256": disposition.source_sha256,
        "source_range_sha256s": list(disposition.source_range_sha256s),
        "discord_message_ids": list(disposition.discord_message_ids),
        "expected_body_sha256": (
            normalized_body_sha256([disposition.expected_body])
            if disposition.kind
            in {DispositionKind.DELIVERED, DispositionKind.SUMMARIZED}
            else None
        ),
        "committed": disposition.committed,
    }


def advance_committed_frontier(
    current: int, dispositions: Sequence[PlannedDisposition]
) -> int:
    if (
        isinstance(current, bool)
        or not isinstance(current, int)
        or current < 0
        or current > MAX_U64
    ):
        raise ValueError("invalid current frontier")
    frontier = current
    seen_ranges: set[str] = set()
    pending_seen = False
    for disposition in dispositions:
        if (
            not isinstance(disposition.kind, DispositionKind)
            or not isinstance(disposition.committed, bool)
        ):
            raise ValueError("unknown disposition or commit state")
        reason = disposition.policy_reason
        reason_valid = False
        if isinstance(reason, str):
            if disposition.kind is DispositionKind.DELIVERED:
                reason_valid = reason == "assistant_text_must_deliver" or bool(
                    re.fullmatch(r"[A-Za-z0-9._-]+:tool_within_bound", reason)
                )
            elif disposition.kind is DispositionKind.SUMMARIZED:
                reason_valid = bool(
                    re.fullmatch(r"[A-Za-z0-9._-]+:tool_bulk_overflow", reason)
                )
            else:
                reason_valid = bool(
                    re.fullmatch(r"[A-Za-z0-9._-]+:control", reason)
                )
        if not reason_valid:
            raise ValueError("invalid disposition policy reason")
        message_ids = disposition.discord_message_ids
        if disposition.committed and disposition.kind in {
            DispositionKind.DELIVERED,
            DispositionKind.SUMMARIZED,
        }:
            if (
                not isinstance(message_ids, tuple)
                or len(message_ids) != 1
                or not _discord_message_id_valid(message_ids[0])
            ):
                raise ValueError("committed delivery lacks Discord evidence")
        elif message_ids:
            raise ValueError("disposition has invalid Discord evidence")
        if (
            isinstance(disposition.start, bool)
            or not isinstance(disposition.start, int)
            or isinstance(disposition.end, bool)
            or not isinstance(disposition.end, int)
            or disposition.start < 0
            or disposition.end <= disposition.start
            or disposition.end > MAX_U64
        ):
            raise ValueError("invalid disposition range")
        if (
            not isinstance(disposition.range_ids, tuple)
            or not disposition.range_ids
            or any(
                not isinstance(range_id, str) or not range_id
                for range_id in disposition.range_ids
            )
        ):
            raise ValueError("invalid disposition range ids")
        if any(range_id in seen_ranges for range_id in disposition.range_ids):
            raise ValueError("duplicate disposition range id")
        seen_ranges.update(disposition.range_ids)
        if not disposition.committed:
            pending_seen = True
            continue
        if pending_seen:
            raise ValueError("committed disposition after pending suffix")
        if disposition.start < frontier:
            raise ValueError("duplicate or overlapping committed span")
        if disposition.start > frontier:
            raise ValueError("committed frontier gap")
        frontier = disposition.end
    return frontier


def _validate_source_ranges(ranges: Sequence[SourceRange], start_cursor: int) -> None:
    cursor = start_cursor
    seen: set[str] = set()
    for entry in ranges:
        if (
            not isinstance(entry.range_id, str)
            or not entry.range_id
            or entry.range_id in seen
        ):
            raise ValueError("duplicate range id")
        seen.add(entry.range_id)
        if (
            not isinstance(entry.kind, RangeKind)
            or isinstance(entry.start, bool)
            or not isinstance(entry.start, int)
            or isinstance(entry.end, bool)
            or not isinstance(entry.end, int)
            or entry.start < 0
            or entry.end > MAX_U64
            or entry.start != cursor
            or entry.end <= entry.start
        ):
            raise ValueError("source ranges must be contiguous and nonempty")
        if entry.end - entry.start != len(entry.raw):
            raise ValueError("cursor range must equal UTF-8 byte length")
        if entry.marker is not None and (
            not isinstance(entry.marker, str)
            or MARKER_RE.fullmatch(entry.marker) is None
        ):
            raise ValueError("invalid E2E marker")
        if entry.marker is not None and entry.marker not in entry.body:
            raise ValueError("E2E marker must be present in its source range")
        cursor = entry.end
    marker_counts: dict[str, int] = {}
    for marker in MARKER_SCAN_RE.findall("".join(entry.body for entry in ranges)):
        marker_counts[marker] = marker_counts.get(marker, 0) + 1
    if any(count > 1 for count in marker_counts.values()):
        raise ValueError("duplicate E2E marker in actual source")


def _discord_message_id_valid(value: Any) -> bool:
    if not isinstance(value, str) or re.fullmatch(r"[0-9]+", value) is None:
        return False
    if value.startswith("0"):
        return False
    maximum = str(MAX_U64)
    return (
        len(value) < len(maximum)
        or (len(value) == len(maximum) and value <= maximum)
    )


def _hash_ranges(ranges: Iterable[SourceRange]) -> str:
    digest = hashlib.sha256()
    for entry in ranges:
        raw = entry.raw
        digest.update(len(raw).to_bytes(8, "big"))
        digest.update(raw)
    return digest.hexdigest()


def _tool_summary(
    ranges: Sequence[SourceRange],
    policy_version: str,
    *,
    max_bytes: int | None = None,
) -> str:
    raw_bytes = sum(len(entry.raw) for entry in ranges)
    markers = [entry.marker for entry in ranges if entry.marker]
    marker_suffix = "" if not markers else " markers=" + ",".join(markers)
    summary = (
        f"[AgentDesk summary policy={policy_version} "
        f"range={ranges[0].start}:{ranges[-1].end} records={len(ranges)} "
        f"raw_bytes={raw_bytes} sha256={_hash_ranges(ranges)}{marker_suffix}]"
    )
    if max_bytes is None or len(summary.encode("utf-8")) <= max_bytes or not markers:
        return summary
    marker_digest = hashlib.sha256("\n".join(markers).encode("utf-8")).hexdigest()
    return (
        f"[AgentDesk summary policy={policy_version} "
        f"range={ranges[0].start}:{ranges[-1].end} records={len(ranges)} "
        f"raw_bytes={raw_bytes} sha256={_hash_ranges(ranges)} "
        f"marker_count={len(markers)} marker_sha256={marker_digest}]"
    )


def normalized_body_sha256(texts: Sequence[str]) -> str:
    digest = hashlib.sha256()
    for text in texts:
        raw = text.encode("utf-8")
        digest.update(len(raw).to_bytes(8, "big"))
        digest.update(raw)
    return digest.hexdigest()


def validate_evidence_bundle(
    bundle: Mapping[str, Any],
    *,
    pinned_manifest: Mapping[str, Any],
    actual_source_id: str,
    actual_source_bytes: bytes,
    discord_channel_id: str,
    discord_after_message_id: str,
) -> list[str]:
    """Recompute the oracle from external pins and actual source bytes."""

    errors: list[str] = []
    manifest = bundle.get("manifest")
    ranges = bundle.get("source_ranges")
    dispositions = bundle.get("dispositions")
    messages = bundle.get("discord_observations")
    delivery = bundle.get("delivery_observation")
    unrelated = bundle.get("unrelated_sessions")
    if not all(
        [
            isinstance(manifest, dict),
            isinstance(ranges, list),
            isinstance(dispositions, list),
            isinstance(messages, list),
            isinstance(delivery, dict),
            isinstance(unrelated, list),
            isinstance(pinned_manifest, Mapping),
            isinstance(actual_source_bytes, bytes),
            isinstance(actual_source_id, str),
            isinstance(discord_channel_id, str),
            isinstance(discord_after_message_id, str),
        ]
    ):
        return ["bundle_shape_invalid"]

    required_manifest = {
        "schema_version",
        "run_id",
        "case_id",
        "provider",
        "channel_id",
        "base_sha",
        "binary_sha",
        "policy_version",
        "normalization_version",
        "actual_source_id",
        "source_generation",
        "coordinate_space",
        "source_start",
        "source_end",
        "durable_frontier_before",
        "durable_anchor_before",
        "durable_anchor_after",
        "discord_complete",
        "bot_author_id",
        "discord_after_message_id",
        "started_at",
    }
    if set(manifest) != required_manifest or set(pinned_manifest) != required_manifest:
        errors.append("manifest_fields_or_schema_mismatch")
        return errors
    if manifest != dict(pinned_manifest):
        errors.append("manifest_not_externally_pinned")
    if manifest["schema_version"] != SCHEMA_VERSION:
        errors.append("manifest_schema_unknown")
    if manifest["provider"] not in ("claude", "codex"):
        errors.append("manifest_provider_unknown")
    if (
        isinstance(manifest["source_generation"], bool)
        or not isinstance(manifest["source_generation"], int)
        or not 0 <= manifest["source_generation"] <= MAX_U64
    ):
        errors.append("manifest_source_generation_invalid")
    if not all(
        isinstance(manifest[key], str) and manifest[key]
        for key in (
            "run_id",
            "case_id",
            "policy_version",
            "actual_source_id",
            "durable_anchor_before",
            "durable_anchor_after",
            "started_at",
        )
    ):
        errors.append("manifest_text_identity_invalid")
    if manifest["durable_anchor_before"] == manifest["durable_anchor_after"]:
        errors.append("manifest_durable_anchor_did_not_advance")
    for key in ("channel_id", "bot_author_id", "discord_after_message_id"):
        if not _discord_message_id_valid(manifest[key]):
            errors.append("manifest_discord_id_invalid")
    for key in ("base_sha", "binary_sha"):
        value = manifest[key]
        if (
            not isinstance(value, str)
            or len(value) != 40
            or any(character not in "0123456789abcdef" for character in value)
        ):
            errors.append("manifest_git_sha_invalid")
    if manifest["coordinate_space"] != "raw_utf8_bytes":
        errors.append("coordinate_space_unknown")
    if manifest["normalization_version"] != "discord-body-v1":
        errors.append("normalization_version_unknown")
    if manifest["discord_complete"] is not True:
        errors.append("discord_fetch_incomplete")
    if (
        manifest["actual_source_id"] != actual_source_id
        or manifest["channel_id"] != discord_channel_id
        or manifest["discord_after_message_id"] != discord_after_message_id
    ):
        errors.append("external_source_or_discord_scope_mismatch")
    source_start = manifest["source_start"]
    source_end = manifest["source_end"]
    durable_frontier_before = manifest["durable_frontier_before"]
    if (
        isinstance(source_start, bool)
        or not isinstance(source_start, int)
        or isinstance(source_end, bool)
        or not isinstance(source_end, int)
        or source_start < 0
        or source_end <= source_start
        or source_end > MAX_U64
        or source_end - source_start != len(actual_source_bytes)
    ):
        errors.append("actual_source_extent_mismatch")
        return errors
    if (
        isinstance(durable_frontier_before, bool)
        or not isinstance(durable_frontier_before, int)
        or durable_frontier_before < 0
        or durable_frontier_before > MAX_U64
        or durable_frontier_before != source_start
    ):
        errors.append("manifest_durable_frontier_invalid")
        return errors

    source_by_id: dict[str, SourceRange] = {}
    source_order: list[str] = []
    source_hashes: dict[str, str] = {}
    cursor = source_start
    actual_markers = MARKER_SCAN_RE.findall(
        actual_source_bytes.decode("utf-8", errors="replace")
    )
    actual_marker_counts: dict[str, int] = {}
    for marker in actual_markers:
        actual_marker_counts[marker] = actual_marker_counts.get(marker, 0) + 1
    if any(count > 1 for count in actual_marker_counts.values()):
        errors.append("source_marker_duplicate")
    markers: dict[str, str] = {}
    declared_markers: set[str] = set()
    for entry in ranges:
        if not isinstance(entry, dict):
            errors.append("source_range_invalid")
            continue
        range_id = entry.get("range_id")
        if not isinstance(range_id, str) or not range_id or range_id in source_by_id:
            errors.append("source_range_id_duplicate_or_invalid")
            continue
        start = entry.get("start")
        end = entry.get("end")
        if (
            isinstance(start, bool)
            or not isinstance(start, int)
            or isinstance(end, bool)
            or not isinstance(end, int)
            or end <= start
            or start < source_start
            or end > source_end
        ):
            errors.append("source_range_empty_or_invalid")
            continue
        if start != cursor:
            errors.append("source_range_gap_or_order")
        cursor = end
        try:
            kind = RangeKind(entry.get("kind"))
        except (TypeError, ValueError):
            errors.append("source_range_kind_unknown")
            continue
        raw = actual_source_bytes[start - source_start : end - source_start]
        try:
            body = raw.decode("utf-8")
        except UnicodeDecodeError:
            errors.append("source_range_not_utf8")
            continue
        marker = entry.get("marker")
        if marker is not None:
            match = MARKER_RE.fullmatch(marker) if isinstance(marker, str) else None
            if (
                match is None
                or match.group("run") != manifest["run_id"]
                or match.group("case") != manifest["case_id"]
            ):
                errors.append("source_marker_invalid")
            elif marker in declared_markers:
                errors.append("source_marker_duplicate")
            else:
                declared_markers.add(marker)
            if not isinstance(marker, str) or marker not in body:
                errors.append("source_marker_bytes_mismatch")
            if entry.get("marker_in_source") is not True:
                errors.append("source_marker_not_witnessed")
        elif entry.get("marker_in_source") not in {None, False}:
            errors.append("source_marker_not_witnessed")
        model_range = SourceRange(range_id, start, end, kind, body, marker)
        digest = _hash_ranges([model_range])
        if entry.get("sha256") != digest:
            errors.append("source_range_hash_mismatch")
        source_by_id[range_id] = model_range
        source_hashes[range_id] = digest
        source_order.append(range_id)
    if cursor != source_end:
        errors.append("source_end_mismatch")
    for marker in actual_marker_counts:
        match = MARKER_RE.fullmatch(marker)
        if (
            match is None
            or match.group("run") != manifest["run_id"]
            or match.group("case") != manifest["case_id"]
        ):
            errors.append("source_marker_invalid")
        if marker not in declared_markers:
            errors.append("source_marker_metadata_missing")
        markers[marker] = "actual_source"

    message_revisions: dict[str, list[Mapping[str, Any]]] = {}
    cutoff_valid = _discord_message_id_valid(discord_after_message_id)
    for message in messages:
        if not isinstance(message, dict) or not isinstance(message.get("message_id"), str):
            errors.append("discord_message_invalid")
            continue
        message_id = message["message_id"]
        if (
            not _discord_message_id_valid(message_id)
            or not cutoff_valid
            or (
                _discord_message_id_valid(message_id)
                and cutoff_valid
                and int(message_id) <= int(discord_after_message_id)
            )
        ):
            errors.append("discord_message_outside_cutoff")
        if message.get("channel_id") != discord_channel_id:
            errors.append("discord_message_channel_mismatch")
        raw_body = message.get("raw_body")
        if not isinstance(raw_body, str):
            errors.append("discord_raw_body_missing")
        else:
            if manifest["normalization_version"] == "discord-body-v1":
                normalized = _normalize_discord_body(
                    raw_body, manifest["normalization_version"]
                )
                if message.get("normalized_body") != normalized:
                    errors.append("discord_normalization_mismatch")
        message_revisions.setdefault(message_id, []).append(message)
    final_messages: dict[str, Mapping[str, Any]] = {}
    for message_id, revisions in message_revisions.items():
        revision_indexes = [item.get("revision_index") for item in revisions]
        if any(
            isinstance(value, bool) or not isinstance(value, int)
            for value in revision_indexes
        ):
            errors.append("discord_revision_gap")
            ordered = revisions
        else:
            ordered = sorted(revisions, key=lambda item: item["revision_index"])
            if [item["revision_index"] for item in ordered] != list(
                range(len(ordered))
            ):
                errors.append("discord_revision_gap")
        final_messages[message_id] = ordered[-1]

    for marker in markers:
        marker_count = sum(
            str(message.get("normalized_body", "")).count(marker)
            for message in final_messages.values()
            if message.get("message_role") == "relay_body"
        )
        if marker_count != 1:
            errors.append(f"marker_count:{marker}:{marker_count}")

    range_owners: dict[str, int] = {}
    used_message_ids: set[str] = set()
    committed_spans: list[tuple[int, int]] = []
    pending_seen = False
    for index, disposition in enumerate(dispositions):
        if not isinstance(disposition, dict):
            errors.append("disposition_invalid")
            continue
        range_ids = disposition.get("range_ids")
        if (
            not isinstance(range_ids, list)
            or not range_ids
            or any(not isinstance(range_id, str) for range_id in range_ids)
        ):
            errors.append("disposition_range_ids_invalid")
            continue
        entries: list[SourceRange] = []
        indexes: list[int] = []
        for range_id in range_ids:
            if range_id in range_owners:
                errors.append("source_range_multiple_dispositions")
            range_owners[range_id] = index
            entry = source_by_id.get(range_id)
            if entry is None:
                errors.append("disposition_unknown_range")
            else:
                entries.append(entry)
                indexes.append(source_order.index(range_id))
        if not entries:
            continue
        if indexes != list(range(indexes[0], indexes[0] + len(indexes))):
            errors.append("disposition_ranges_noncontiguous_or_reordered")
        try:
            kind = DispositionKind(disposition.get("kind"))
        except (TypeError, ValueError):
            errors.append("disposition_kind_unknown")
            continue
        source_kinds = {entry.kind for entry in entries}
        policy = manifest["policy_version"]
        expected_reason: str | None = None
        if kind is DispositionKind.DELIVERED and source_kinds == {
            RangeKind.ASSISTANT_TEXT
        }:
            expected_reason = "assistant_text_must_deliver"
        elif kind is DispositionKind.DELIVERED and source_kinds == {RangeKind.TOOL_BULK}:
            expected_reason = f"{policy}:tool_within_bound"
        elif kind is DispositionKind.SUMMARIZED and source_kinds == {
            RangeKind.TOOL_BULK
        }:
            expected_reason = f"{policy}:tool_bulk_overflow"
        elif kind is DispositionKind.OMITTED_POLICY and source_kinds == {
            RangeKind.CONTROL
        }:
            expected_reason = f"{policy}:control"
        if expected_reason is None or disposition.get("policy_reason") != expected_reason:
            errors.append("disposition_policy_mismatch")
        if RangeKind.ASSISTANT_TEXT in source_kinds and kind is not DispositionKind.DELIVERED:
            errors.append("assistant_text_not_delivered")
        if kind is DispositionKind.OMITTED_POLICY and source_kinds != {RangeKind.CONTROL}:
            errors.append("noncontrol_policy_omission")
        if kind is DispositionKind.SUMMARIZED and source_kinds != {RangeKind.TOOL_BULK}:
            errors.append("summary_not_tool_bulk")
        expected_hashes = [source_hashes[entry.range_id] for entry in entries]
        if disposition.get("source_sha256") != _hash_ranges(entries):
            errors.append("disposition_source_hash_mismatch")
        if disposition.get("source_range_sha256s") != expected_hashes:
            errors.append("disposition_source_hashes_mismatch")

        if kind is DispositionKind.SUMMARIZED:
            expected_body = _tool_summary(entries, policy)
        elif kind is DispositionKind.DELIVERED:
            expected_body = "".join(entry.body for entry in entries)
        else:
            expected_body = ""
        expected_body_hash = (
            normalized_body_sha256([expected_body])
            if kind in {DispositionKind.DELIVERED, DispositionKind.SUMMARIZED}
            else None
        )
        if disposition.get("expected_body_sha256") != expected_body_hash:
            errors.append("expected_body_hash_not_recomputed")

        committed = disposition.get("committed")
        if not isinstance(committed, bool):
            errors.append("disposition_commit_state_invalid")
            continue
        if not committed:
            pending_seen = True
            continue
        if pending_seen:
            errors.append("committed_disposition_after_pending_suffix")
        span = (entries[0].start, entries[-1].end)
        if span in committed_spans or any(
            span[0] < existing[1] and existing[0] < span[1]
            for existing in committed_spans
        ):
            errors.append("duplicate_or_overlapping_committed_span")
        committed_spans.append(span)
        ids = disposition.get("discord_message_ids", [])
        if not isinstance(ids, list):
            errors.append("disposition_message_ids_invalid")
            continue
        if kind in {DispositionKind.DELIVERED, DispositionKind.SUMMARIZED}:
            if len(ids) != 1:
                errors.append("committed_delivery_requires_one_message_id")
                continue
            message_id = ids[0]
            if not _discord_message_id_valid(message_id):
                errors.append("discord_message_id_invalid")
                continue
            if message_id in used_message_ids:
                errors.append("discord_message_reused")
            used_message_ids.add(message_id)
            message = final_messages.get(message_id)
            if message is None:
                errors.append("discord_message_not_observed")
                continue
            if message.get("author_id") != manifest["bot_author_id"]:
                errors.append("discord_message_author_mismatch")
            if message.get("message_role") != "relay_body":
                errors.append("discord_message_wrong_role")
            if normalized_body_sha256(
                [str(message.get("normalized_body", ""))]
            ) != expected_body_hash:
                errors.append("discord_body_hash_mismatch")
        elif ids:
            errors.append("omission_has_discord_message")

    if set(range_owners) != set(source_by_id):
        errors.append("source_range_without_disposition")
    committed_frontier = durable_frontier_before
    for start, end in committed_spans:
        if start != committed_frontier:
            errors.append("committed_frontier_gap_or_order")
            break
        committed_frontier = end
    if (
        delivery.get("source_id") != actual_source_id
        or delivery.get("source_generation") != manifest["source_generation"]
    ):
        errors.append("delivery_source_identity_mismatch")
    durable_anchor = delivery.get("durable_anchor")
    if not isinstance(durable_anchor, str) or not durable_anchor:
        errors.append("durable_anchor_missing")
    elif durable_anchor != manifest["durable_anchor_after"]:
        errors.append("durable_anchor_not_externally_pinned")
    delivered_frontier = delivery.get("committed_frontier")
    if (
        isinstance(delivered_frontier, bool)
        or not isinstance(delivered_frontier, int)
        or delivered_frontier < 0
        or delivered_frontier > MAX_U64
        or delivered_frontier != committed_frontier
    ):
        errors.append("durable_frontier_mismatch")
    if committed_frontier != source_end:
        errors.append("pending_source_suffix")

    providers: set[str] = set()
    for entry in unrelated:
        if not isinstance(entry, dict):
            errors.append("unrelated_session_evidence_invalid")
            continue
        provider = entry.get("provider")
        if provider not in ("claude", "codex"):
            errors.append("unrelated_provider_unknown")
            continue
        providers.add(provider)
        if (
            entry.get("channel_id") == discord_channel_id
            or entry.get("identity_complete") is not True
            or not isinstance(entry.get("observed_at"), str)
            or not entry.get("observed_at")
        ):
            errors.append("unrelated_session_evidence_invalid")
        if entry.get("relay_gap") is not False or entry.get("regression") is not False:
            errors.append("unrelated_session_regression")
    if providers != {"claude", "codex"}:
        errors.append("unrelated_provider_evidence_missing")
    return errors


def _normalize_discord_body(raw_body: str, version: str) -> str:
    if version != "discord-body-v1":
        raise ValueError("unknown normalization version")
    return raw_body.replace("\r\n", "\n")

from __future__ import annotations

import copy
import hashlib
import json
import sys
import unittest
from dataclasses import replace
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from model import (  # noqa: E402
    AllowedAction,
    ClockStamp,
    DeliveryProof,
    DispositionKind,
    InflightContinuity,
    LedgerOutcome,
    LedgerState,
    MAX_U64,
    Observation,
    QueueDisposition,
    RangeKind,
    SourceRange,
    TerminationProof,
    Verdict,
    advance_committed_frontier,
    assess,
    clock_allows_transition,
    commit_dispositions,
    load_ledger_json,
    mark_effect_pending,
    normalized_body_sha256,
    plan_bounded_batch,
    recover_after_crash,
    reserve_attempt,
    serialize_planned_disposition,
    settle_effect,
    validate_evidence_bundle,
)


FIXTURES = ROOT / "fixtures" / "incidents.json"


def fixture_data() -> dict:
    return json.loads(FIXTURES.read_text(encoding="utf-8"))


def observation_from_fixture(raw: dict) -> Observation:
    values = dict(raw)
    values["continuity"] = InflightContinuity(values.get("continuity", "present_stable"))
    values["queue"] = QueueDisposition(values.get("queue", "active"))
    if isinstance(values.get("observed_at"), dict):
        values["observed_at"] = ClockStamp(**values["observed_at"])
    if isinstance(values.get("termination_proof"), dict):
        proof = dict(values["termination_proof"])
        proof["queue"] = QueueDisposition(proof["queue"])
        proof["action_reprobe_at"] = ClockStamp(**proof["action_reprobe_at"])
        values["termination_proof"] = TerminationProof(**proof)
    return Observation(**values)


def repair_observation(
    episode: str = "ep-ledger",
    *,
    continuity: InflightContinuity = InflightContinuity.PRESENT_STABLE,
    source_id: str = "source-ledger",
    source_generation: int = 1,
    durable_anchor: str = "anchor-10",
    observed_at: ClockStamp | None = None,
    delivery_progress: bool = False,
    queue: QueueDisposition = QueueDisposition.ACTIVE,
) -> Observation:
    return Observation(
        episode_key=episode,
        source_id=source_id,
        source_generation=source_generation,
        durable_anchor=durable_anchor,
        observed_at=observed_at,
        continuity=continuity,
        queue=queue,
        source_progress=True,
        delivery_progress=delivery_progress,
        control_contradiction=True,
    )


def delivery_proof(
    episode: str,
    *,
    now: ClockStamp,
    source_frontier: int,
    committed_frontier: int,
    source_id: str = "source-ledger",
    source_generation: int = 1,
    durable_anchor: str | None = None,
    queue: QueueDisposition = QueueDisposition.ACTIVE,
) -> DeliveryProof:
    return DeliveryProof(
        episode_key=episode,
        source_id=source_id,
        source_generation=source_generation,
        durable_anchor=durable_anchor or f"anchor-{committed_frontier}",
        source_frontier=source_frontier,
        committed_frontier=committed_frontier,
        queue=queue,
        observed_at=now,
    )


def execute_fault_fixture(case: dict) -> dict:
    operation = case["operation"]
    inputs = case["input"]
    if operation in {"recover_reserved", "recover_pending", "pending_continuity"}:
        episode = inputs["episode"]
        reserved_at = ClockStamp(10_000, "fixture-boot", 100)
        observation = repair_observation(
            episode,
            observed_at=reserved_at,
        )
        reserved = reserve_attempt(
            None,
            observation,
            attempt_id="fixture-attempt",
            delivery_proof=delivery_proof(
                episode,
                now=reserved_at,
                source_frontier=inputs.get("source", 100),
                committed_frontier=inputs.get("delivered", 10),
            ),
            now=reserved_at,
        ).record
        if operation == "recover_reserved":
            transition = recover_after_crash(
                reserved,
                repair_observation(episode),
                attempt_id=reserved.attempt_id,
            )
        else:
            pending = mark_effect_pending(
                reserved,
                attempt_id=reserved.attempt_id,
                now=ClockStamp(10_001, "fixture-boot", 101),
            ).record
            if operation == "recover_pending":
                transition = recover_after_crash(
                    pending, attempt_id=pending.attempt_id
                )
            else:
                now = ClockStamp(10_002, "fixture-boot", 102)
                continuity = InflightContinuity(inputs["continuity"])
                transition = settle_effect(
                    pending,
                    repair_observation(
                        episode,
                        continuity=continuity,
                        observed_at=now,
                    ),
                    attempt_id=pending.attempt_id,
                    delivery_proof=delivery_proof(
                        episode,
                        now=now,
                        source_frontier=101,
                        committed_frontier=10,
                    ),
                    now=now,
                )
        return {
            "changed": transition.changed,
            "state": transition.record.state.value,
            "attempts_spent": transition.record.attempts_spent,
            "must_reprobe": transition.must_reprobe,
        }
    if operation == "circuit_rearm":
        episode = inputs["episode"]
        record = None
        for index in range(2):
            reserve_now = ClockStamp(
                1_000_000 + index * 700_000,
                "fixture-boot",
                10_000 + index * 700_000,
            )
            reserve_anchor = "anchor-10"
            reserved = reserve_attempt(
                record,
                repair_observation(
                    episode,
                    observed_at=reserve_now,
                    durable_anchor=reserve_anchor,
                ),
                attempt_id=f"fixture-attempt-{index}",
                delivery_proof=delivery_proof(
                    episode,
                    now=reserve_now,
                    source_frontier=100 + index * 100,
                    committed_frontier=10,
                    durable_anchor=reserve_anchor,
                ),
                now=reserve_now,
            ).record
            pending = mark_effect_pending(
                reserved,
                attempt_id=reserved.attempt_id,
                now=ClockStamp(
                    reserve_now.wall_ms + 1,
                    reserve_now.boot_id,
                    reserve_now.monotonic_ms + 1,
                ),
            ).record
            settle_now = ClockStamp(
                reserve_now.wall_ms + 10,
                reserve_now.boot_id,
                reserve_now.monotonic_ms + 10,
            )
            record = settle_effect(
                pending,
                repair_observation(
                    episode,
                    observed_at=settle_now,
                    durable_anchor=reserve_anchor,
                ),
                attempt_id=pending.attempt_id,
                delivery_proof=delivery_proof(
                    episode,
                    now=settle_now,
                    source_frontier=200 + index * 100,
                    committed_frontier=10,
                    durable_anchor=reserve_anchor,
                ),
                now=settle_now,
            ).record
        delivered_after = inputs["delivered_after"]
        rearm_now = ClockStamp(3_000_000, "fixture-boot", 2_010_000)
        anchor = f"anchor-{delivered_after}"
        transition = reserve_attempt(
            record,
            repair_observation(
                episode,
                observed_at=rearm_now,
                durable_anchor=anchor,
                delivery_progress=delivered_after > record.baseline_delivered,
            ),
            attempt_id="fixture-attempt-rearm",
            delivery_proof=delivery_proof(
                episode,
                now=rearm_now,
                source_frontier=999,
                committed_frontier=delivered_after,
                durable_anchor=anchor,
            ),
            now=rearm_now,
        )
        return {
            "changed": transition.changed,
            "reason": transition.reason,
            "lifetime_no_effect_count": transition.record.lifetime_no_effect_count,
        }
    if operation == "clock_transition":
        return {
            "allowed": clock_allows_transition(
                ClockStamp(**inputs["previous"]),
                ClockStamp(**inputs["current"]),
                trusted_now=ClockStamp(**inputs["trusted_now"]),
                max_future_skew_ms=inputs["max_future_skew_ms"],
            )
        }
    if operation == "load_ledger":
        loaded = load_ledger_json(
            inputs["raw"], trusted_now=ClockStamp(20_000, "fixture-boot", 200)
        )
        return {"fail_closed": loaded.fail_closed, "reason": loaded.reason}
    if operation == "plan_single_range":
        body = "x" * inputs["body_bytes"]
        source_range = SourceRange(
            "fixture-range",
            0,
            len(body),
            RangeKind(inputs["kind"]),
            body,
        )
        plan = plan_bounded_batch(
            [source_range],
            start_cursor=0,
            max_pending_items=inputs["max_items"],
            max_pending_bytes=inputs["max_bytes"],
        )
        result = {
            "processed_end": plan.processed_end,
            "blocked_reason": plan.blocked_reason,
            "dispositions": len(plan.dispositions),
        }
        if plan.dispositions:
            committed = commit_dispositions(plan, {0: ["9001"]})
            result["committed_frontier"] = advance_committed_frontier(0, committed)
        return result
    raise AssertionError(f"unknown executable fixture operation: {operation}")


class AuthorityFixtureTests(unittest.TestCase):
    def test_incident_authority_matrix(self) -> None:
        for case in fixture_data()["authority_cases"]:
            with self.subTest(case=case["name"]):
                result = assess(observation_from_fixture(case["observation"]))
                self.assertEqual(
                    None if result.candidate is None else result.candidate.value,
                    case["expected_candidate"],
                )
                self.assertEqual(result.action.value, case["expected_action"])
                self.assertEqual(result.may_spend_automatic_action, case["expected_spend"])

    def test_producer_dead_requires_episode_bound_proof_and_safe_queue(self) -> None:
        now = ClockStamp(1_000, "boot-proof", 10)
        for has_proof in (False, True):
            for queue in QueueDisposition:
                proof = (
                    TerminationProof(
                        episode_key="ep-proof-matrix",
                        source_id="source-proof",
                        source_generation=7,
                        durable_anchor="anchor-proof",
                        queue=queue,
                        termination_id="termination-proof",
                        finalizer_committed=True,
                        watcher_stopped=True,
                        action_reprobe_at=now,
                    )
                    if has_proof
                    else None
                )
                result = assess(
                    Observation(
                        episode_key="ep-proof-matrix",
                        source_id="source-proof",
                        source_generation=7,
                        durable_anchor="anchor-proof",
                        observed_at=now,
                        termination_proof=proof,
                        queue=queue,
                    )
                )
                expected_dead = has_proof and queue in {
                    QueueDisposition.IDLE,
                    QueueDisposition.TERMINAL,
                }
                self.assertEqual(result.candidate is Verdict.PRODUCER_DEAD, expected_dead)

    def test_failed_repairs_never_mutate_quiet_into_death(self) -> None:
        for count in (0, 1, 2, 100):
            result = assess(
                Observation(
                    episode_key="ep-failed-repairs",
                    queue=QueueDisposition.ACTIVE,
                    failed_repairs=count,
                )
            )
            self.assertIsNone(result.candidate)
            self.assertEqual(result.action, AllowedAction.NONE)

    def test_boolean_termination_claim_is_not_episode_bound_proof(self) -> None:
        result = assess(
            Observation(
                episode_key="ep-unbound-termination",
                affirmative_termination=True,
                queue=QueueDisposition.TERMINAL,
            )
        )
        self.assertIsNone(result.candidate)
        self.assertEqual(result.action, AllowedAction.NONE)

    def test_termination_proof_requires_exact_identity_and_action_reprobe(self) -> None:
        now = ClockStamp(5_000, "boot-proof", 50)
        proof = TerminationProof(
            episode_key="ep-exact",
            source_id="source-exact",
            source_generation=9,
            durable_anchor="anchor-exact",
            queue=QueueDisposition.TERMINAL,
            termination_id="termination-exact",
            finalizer_committed=True,
            watcher_stopped=True,
            action_reprobe_at=now,
        )

        def observe(candidate: TerminationProof) -> Observation:
            return Observation(
                episode_key="ep-exact",
                source_id="source-exact",
                source_generation=9,
                durable_anchor="anchor-exact",
                observed_at=now,
                queue=QueueDisposition.TERMINAL,
                termination_proof=candidate,
            )

        self.assertEqual(assess(observe(proof)).candidate, Verdict.PRODUCER_DEAD)
        for mutant in (
            replace(proof, episode_key="ep-other"),
            replace(proof, source_id="source-other"),
            replace(proof, source_generation=10),
            replace(proof, durable_anchor="anchor-other"),
            replace(proof, action_reprobe_at=ClockStamp(4_999, "boot-proof", 49)),
            replace(proof, finalizer_committed=False),
            replace(proof, watcher_stopped=False),
        ):
            with self.subTest(mutant=mutant):
                self.assertIsNone(assess(observe(mutant)).candidate)

    def test_untyped_observation_enums_never_authorize_an_action(self) -> None:
        base = Observation(
            episode_key="ep-untyped",
            source_progress=True,
            control_contradiction=True,
        )
        for mutant in (
            replace(base, continuity="present_stable"),
            replace(base, queue="active"),
        ):
            with self.subTest(mutant=mutant):
                result = assess(mutant)
                self.assertIsNone(result.candidate)
                self.assertEqual(result.action, AllowedAction.NONE)

    def test_all_observation_and_termination_flags_require_strict_booleans(self) -> None:
        now = ClockStamp(6_000, "boot-strict-bool", 60)
        proof = TerminationProof(
            episode_key="ep-strict-bool",
            source_id="source-strict-bool",
            source_generation=1,
            durable_anchor="anchor-strict-bool",
            queue=QueueDisposition.TERMINAL,
            termination_id="termination-strict-bool",
            finalizer_committed=True,
            watcher_stopped=True,
            action_reprobe_at=now,
        )
        observation = Observation(
            episode_key=proof.episode_key,
            source_id=proof.source_id,
            source_generation=proof.source_generation,
            durable_anchor=proof.durable_anchor,
            observed_at=now,
            queue=proof.queue,
            termination_proof=proof,
        )
        self.assertEqual(assess(observation).candidate, Verdict.PRODUCER_DEAD)

        for field in (
            "identity_complete",
            "clock_valid",
            "affirmative_termination",
            "source_progress",
            "delivery_progress",
            "control_contradiction",
            "terminal_delivery_committed",
            "typed_idle",
        ):
            with self.subTest(owner="observation", field=field):
                result = assess(replace(observation, **{field: "false"}))
                self.assertIsNone(result.candidate)
                self.assertEqual(result.action, AllowedAction.NONE)

        for field in (
            "finalizer_committed",
            "watcher_stopped",
            "identity_complete",
            "clock_valid",
        ):
            with self.subTest(owner="termination_proof", field=field):
                result = assess(
                    replace(observation, termination_proof=replace(proof, **{field: "false"}))
                )
                self.assertIsNone(result.candidate)
                self.assertEqual(result.action, AllowedAction.NONE)


class LedgerFaultTests(unittest.TestCase):
    def setUp(self) -> None:
        self.t0 = ClockStamp(1_000_000, "boot-a", 10_000)

    def _reserve_and_mark(
        self,
        record=None,
        *,
        episode: str = "ep-ledger",
        source: int = 100,
        delivered: int = 10,
        wall: int = 1_000_000,
        mono: int = 10_000,
        attempt: str = "attempt-1",
    ):
        reserved_at = ClockStamp(wall, "boot-a", mono)
        anchor = f"anchor-{delivered}"
        reserved = reserve_attempt(
            record,
            repair_observation(
                episode,
                durable_anchor=anchor,
                observed_at=reserved_at,
                delivery_progress=(
                    record is not None and delivered > record.baseline_delivered
                ),
            ),
            attempt_id=attempt,
            delivery_proof=delivery_proof(
                episode,
                now=reserved_at,
                source_frontier=source,
                committed_frontier=delivered,
                durable_anchor=anchor,
            ),
            now=reserved_at,
        )
        self.assertTrue(reserved.changed, reserved.reason)
        self.assertEqual(reserved.spent_delta, 0)
        pending = mark_effect_pending(
            reserved.record,
            attempt_id=attempt,
            now=ClockStamp(wall + 1, "boot-a", mono + 1),
        )
        self.assertTrue(pending.changed, pending.reason)
        self.assertEqual(pending.spent_delta, 1)
        return reserved.record, pending.record

    def _settle_pending(
        self,
        pending,
        *,
        source: int,
        delivered: int,
        wall: int,
        mono: int,
        continuity: InflightContinuity = InflightContinuity.PRESENT_STABLE,
    ):
        now = ClockStamp(wall, "boot-a", mono)
        anchor = f"anchor-{delivered}"
        progressed = delivered > pending.baseline_delivered
        return settle_effect(
            pending,
            repair_observation(
                pending.episode_key,
                continuity=continuity,
                durable_anchor=anchor,
                observed_at=now,
                delivery_progress=progressed,
            ),
            attempt_id=pending.attempt_id,
            delivery_proof=delivery_proof(
                pending.episode_key,
                now=now,
                source_frontier=source,
                committed_frontier=delivered,
                durable_anchor=anchor,
            ),
            now=now,
        )

    def test_reserved_crash_cancels_without_action_spend(self) -> None:
        reserved, _ = self._reserve_and_mark()
        recovered = recover_after_crash(
            reserved, repair_observation(), attempt_id=reserved.attempt_id
        )
        self.assertEqual(recovered.record.state, LedgerState.SETTLED)
        self.assertEqual(recovered.record.outcome, LedgerOutcome.INCONCLUSIVE)
        self.assertEqual(recovered.record.attempts_spent, 0)
        self.assertFalse(recovered.must_reprobe)

    def test_reserved_crash_waits_for_exact_stable_reprobe(self) -> None:
        reserved, _ = self._reserve_and_mark()
        for observation in (
            None,
            repair_observation(continuity=InflightContinuity.MISSING),
            repair_observation("ep-successor"),
        ):
            recovered = recover_after_crash(
                reserved, observation, attempt_id=reserved.attempt_id
            )
            self.assertFalse(recovered.changed)
            self.assertTrue(recovered.must_reprobe)
            self.assertEqual(recovered.record.state, LedgerState.RESERVED)

    def test_effect_pending_crash_never_replays_without_reprobe(self) -> None:
        _, pending = self._reserve_and_mark()
        recovered = recover_after_crash(pending, attempt_id=pending.attempt_id)
        self.assertFalse(recovered.changed)
        self.assertTrue(recovered.must_reprobe)
        self.assertEqual(recovered.record.attempts_spent, 1)

    def test_effect_pending_cannot_be_overwritten_by_a_new_reservation(self) -> None:
        _, pending = self._reserve_and_mark()
        for episode in (pending.episode_key, "ep-successor-before-settle"):
            now = ClockStamp(1_000_010, "boot-a", 10_010)
            second = reserve_attempt(
                pending,
                repair_observation(
                    episode,
                    durable_anchor="anchor-10",
                    observed_at=now,
                ),
                attempt_id="attempt-2",
                delivery_proof=delivery_proof(
                    episode,
                    now=now,
                    source_frontier=101,
                    committed_frontier=10,
                ),
                now=now,
            )
            with self.subTest(episode=episode):
                self.assertFalse(second.changed)
                self.assertTrue(second.must_reprobe)
                self.assertEqual(second.record, pending)
                self.assertEqual(second.record.attempts_spent, 1)

    def test_attempt_cas_pins_mark_recover_and_settle(self) -> None:
        reserved, pending = self._reserve_and_mark()
        wrong_mark = mark_effect_pending(
            reserved,
            attempt_id="wrong-attempt",
            now=ClockStamp(1_000_001, "boot-a", 10_001),
        )
        self.assertFalse(wrong_mark.changed)
        self.assertTrue(wrong_mark.must_reprobe)
        wrong_recover = recover_after_crash(
            pending, attempt_id="wrong-attempt"
        )
        self.assertFalse(wrong_recover.changed)
        self.assertTrue(wrong_recover.must_reprobe)
        now = ClockStamp(1_000_100, "boot-a", 10_100)
        wrong_settle = settle_effect(
            pending,
            repair_observation(observed_at=now),
            attempt_id="wrong-attempt",
            delivery_proof=delivery_proof(
                pending.episode_key,
                now=now,
                source_frontier=200,
                committed_frontier=10,
            ),
            now=now,
        )
        self.assertFalse(wrong_settle.changed)
        self.assertTrue(wrong_settle.must_reprobe)

    def test_successor_settlement_requires_complete_action_time_identity(self) -> None:
        _, pending = self._reserve_and_mark()
        now = ClockStamp(1_000_100, "boot-a", 10_100)
        incomplete = settle_effect(
            pending,
            Observation(episode_key="ep-successor", observed_at=None),
            attempt_id=pending.attempt_id,
            delivery_proof=None,
            now=now,
        )
        self.assertFalse(incomplete.changed)
        self.assertTrue(incomplete.must_reprobe)
        self.assertEqual(incomplete.reason, "successor_identity_inconclusive")

        complete = settle_effect(
            pending,
            repair_observation(
                "ep-successor",
                continuity=InflightContinuity.REPLACED,
                observed_at=now,
            ),
            attempt_id=pending.attempt_id,
            delivery_proof=None,
            now=now,
        )
        self.assertTrue(complete.changed)
        self.assertEqual(complete.record.outcome, LedgerOutcome.SUPERSEDED)

    def test_replaced_continuity_without_successor_identity_cannot_supersede_or_rearm(
        self,
    ) -> None:
        _, pending = self._reserve_and_mark()
        now = ClockStamp(1_000_100, "boot-a", 10_100)
        replaced_same_identity = settle_effect(
            pending,
            repair_observation(
                pending.episode_key,
                continuity=InflightContinuity.REPLACED,
                source_id=pending.source_id,
                source_generation=pending.source_generation,
                durable_anchor="anchor-rebound-but-not-successor",
                observed_at=now,
            ),
            attempt_id=pending.attempt_id,
            delivery_proof=None,
            now=now,
        )
        self.assertFalse(replaced_same_identity.changed)
        self.assertTrue(replaced_same_identity.must_reprobe)
        self.assertEqual(replaced_same_identity.record, pending)

        reserve_now = ClockStamp(1_000_101, "boot-a", 10_101)
        replacement_attempt = reserve_attempt(
            replaced_same_identity.record,
            repair_observation(
                pending.episode_key,
                durable_anchor=pending.durable_anchor,
                observed_at=reserve_now,
            ),
            attempt_id="attempt-after-false-replacement",
            delivery_proof=delivery_proof(
                pending.episode_key,
                now=reserve_now,
                source_frontier=pending.baseline_source,
                committed_frontier=pending.baseline_delivered,
                durable_anchor=pending.durable_anchor,
            ),
            now=reserve_now,
        )
        self.assertFalse(replacement_attempt.changed)
        self.assertTrue(replacement_attempt.must_reprobe)
        self.assertEqual(replacement_attempt.record, pending)

        actual_successor = settle_effect(
            pending,
            repair_observation(
                pending.episode_key,
                continuity=InflightContinuity.REPLACED,
                source_id=pending.source_id,
                source_generation=pending.source_generation + 1,
                durable_anchor="anchor-successor",
                observed_at=now,
            ),
            attempt_id=pending.attempt_id,
            delivery_proof=None,
            now=now,
        )
        self.assertTrue(actual_successor.changed)
        self.assertEqual(actual_successor.record.outcome, LedgerOutcome.SUPERSEDED)

    def test_delivery_proof_requires_generation_and_anchor_advance(self) -> None:
        _, pending = self._reserve_and_mark()
        now = ClockStamp(1_000_100, "boot-a", 10_100)
        observation = repair_observation(
            durable_anchor=pending.durable_anchor,
            observed_at=now,
            delivery_progress=True,
        )
        unchanged_anchor = settle_effect(
            pending,
            observation,
            attempt_id=pending.attempt_id,
            delivery_proof=delivery_proof(
                pending.episode_key,
                now=now,
                source_frontier=200,
                committed_frontier=11,
                durable_anchor=pending.durable_anchor,
            ),
            now=now,
        )
        self.assertFalse(unchanged_anchor.changed)
        self.assertEqual(unchanged_anchor.reason, "delivery_anchor_did_not_advance")

        wrong_generation = settle_effect(
            pending,
            replace(observation, durable_anchor="anchor-11", source_generation=2),
            attempt_id=pending.attempt_id,
            delivery_proof=delivery_proof(
                pending.episode_key,
                now=now,
                source_frontier=200,
                committed_frontier=11,
                durable_anchor="anchor-11",
                source_generation=2,
            ),
            now=now,
        )
        self.assertFalse(wrong_generation.changed)
        self.assertEqual(
            wrong_generation.reason, "delivery_proof_record_identity_mismatch"
        )

    def test_delivery_proof_requires_action_time_observation_and_possible_frontier(self) -> None:
        _, pending = self._reserve_and_mark()
        now = ClockStamp(1_000_100, "boot-a", 10_100)
        stale = settle_effect(
            pending,
            repair_observation(observed_at=ClockStamp(1_000_099, "boot-a", 10_099)),
            attempt_id=pending.attempt_id,
            delivery_proof=delivery_proof(
                pending.episode_key,
                now=now,
                source_frontier=200,
                committed_frontier=10,
            ),
            now=now,
        )
        self.assertFalse(stale.changed)
        self.assertEqual(stale.reason, "delivery_observation_not_action_time")

        impossible = settle_effect(
            pending,
            repair_observation(
                durable_anchor="anchor-201",
                observed_at=now,
                delivery_progress=True,
            ),
            attempt_id=pending.attempt_id,
            delivery_proof=delivery_proof(
                pending.episode_key,
                now=now,
                source_frontier=200,
                committed_frontier=201,
                durable_anchor="anchor-201",
            ),
            now=now,
        )
        self.assertFalse(impossible.changed)
        self.assertEqual(impossible.reason, "delivery_proof_malformed")

    def test_unproved_integer_frontier_cannot_rearm_open_circuit(self) -> None:
        record = None
        for index in range(2):
            wall = 1_000_000 + index * 700_000
            _, pending = self._reserve_and_mark(
                record,
                source=100 + index * 100,
                wall=wall,
                mono=10_000 + index * 700_000,
                attempt=f"attempt-proof-{index}",
            )
            record = self._settle_pending(
                pending,
                source=200 + index * 100,
                delivered=10,
                wall=wall + 10,
                mono=10_010 + index * 700_000,
            ).record
        self.assertTrue(record.circuit_open)

        unproved = reserve_attempt(
            record,
            repair_observation(
                durable_anchor="anchor-11",
                observed_at=ClockStamp(3_000_000, "boot-a", 2_010_000),
                delivery_progress=True,
            ),
            attempt_id="attempt-unproved-frontier",
            delivery_proof=None,
            now=ClockStamp(3_000_000, "boot-a", 2_010_000),
        )
        self.assertFalse(unproved.changed)
        self.assertEqual(unproved.reason, "delivery_proof_required")

    def test_settle_rejects_frontier_growth_without_valid_identity_and_queue(self) -> None:
        _, pending = self._reserve_and_mark()
        invalid = settle_effect(
            pending,
            Observation(
                episode_key=pending.episode_key,
                source_id=pending.source_id,
                source_generation=pending.source_generation,
                durable_anchor="anchor-11",
                observed_at=ClockStamp(1_000_100, "boot-a", 10_100),
                identity_complete=False,
                clock_valid=False,
                queue=QueueDisposition.UNKNOWN,
                delivery_progress=False,
                control_contradiction=True,
                source_progress=True,
            ),
            attempt_id=pending.attempt_id,
            delivery_proof=delivery_proof(
                pending.episode_key,
                now=ClockStamp(1_000_100, "boot-a", 10_100),
                source_frontier=200,
                committed_frontier=11,
                durable_anchor="anchor-11",
                queue=QueueDisposition.UNKNOWN,
            ),
            now=ClockStamp(1_000_100, "boot-a", 10_100),
        )
        self.assertFalse(invalid.changed)
        self.assertTrue(invalid.must_reprobe)

    def test_4104_missing_or_reappeared_reserves_nothing(self) -> None:
        for continuity in (
            InflightContinuity.MISSING,
            InflightContinuity.REAPPEARED_SAME,
        ):
            transition = reserve_attempt(
                None,
                repair_observation(
                    continuity=continuity,
                    observed_at=self.t0,
                ),
                attempt_id="forbidden",
                delivery_proof=delivery_proof(
                    "ep-ledger",
                    now=self.t0,
                    source_frontier=100,
                    committed_frontier=10,
                ),
                now=self.t0,
            )
            self.assertFalse(transition.changed)
            self.assertEqual(transition.spent_delta, 0)
            self.assertIsNone(transition.record)

    def test_4104_missing_pending_freezes_without_settlement_or_respend(self) -> None:
        _, pending = self._reserve_and_mark()
        frozen = self._settle_pending(
            pending,
            source=200,
            delivered=10,
            wall=1_000_100,
            mono=10_100,
            continuity=InflightContinuity.MISSING,
        )
        self.assertFalse(frozen.changed)
        self.assertTrue(frozen.must_reprobe)
        self.assertEqual(frozen.record, pending)
        self.assertEqual(frozen.spent_delta, 0)

    def test_unknown_continuity_cannot_settle_or_rearm(self) -> None:
        _, pending = self._reserve_and_mark()
        now = ClockStamp(1_000_010, "boot-a", 10_010)
        observation = replace(
            repair_observation(
                pending.episode_key,
                durable_anchor="anchor-11",
                observed_at=now,
                delivery_progress=True,
            ),
            continuity="future_continuity",
        )
        transition = settle_effect(
            pending,
            observation,
            attempt_id=pending.attempt_id,
            delivery_proof=delivery_proof(
                pending.episode_key,
                now=now,
                source_frontier=101,
                committed_frontier=11,
                durable_anchor="anchor-11",
            ),
            now=now,
        )
        self.assertFalse(transition.changed)
        self.assertTrue(transition.must_reprobe)
        self.assertEqual(transition.reason, "continuity_not_typed")
        self.assertEqual(transition.record.state, LedgerState.EFFECT_PENDING)
        self.assertEqual(transition.record.baseline_delivered, 10)

    def test_4104_e22_missing_reappearing_sequence_preserves_episode(self) -> None:
        episode = "ep-4104-e22"
        _, pending = self._reserve_and_mark(episode=episode, attempt="attempt-e22")
        original = pending
        for offset, continuity in enumerate(
            (
                InflightContinuity.MISSING,
                InflightContinuity.REAPPEARED_SAME,
            )
        ):
            frozen = self._settle_pending(
                pending,
                source=150 + offset,
                delivered=10,
                wall=1_000_100 + offset,
                mono=10_100 + offset,
                continuity=continuity,
            )
            self.assertFalse(frozen.changed)
            self.assertTrue(frozen.must_reprobe)
            self.assertEqual(frozen.record, original)
            self.assertEqual(frozen.record.attempts_spent, 1)
            self.assertEqual(frozen.record.failure_streak, 0)
            pending = frozen.record

        completed = self._settle_pending(
            pending,
            source=200,
            delivered=11,
            wall=1_000_102,
            mono=10_102,
        )
        self.assertTrue(completed.changed)
        self.assertEqual(completed.record.state, LedgerState.SETTLED)
        self.assertEqual(
            completed.record.outcome, LedgerOutcome.VERIFIED_DELIVERY_PROGRESS
        )
        self.assertEqual(completed.record.attempts_spent, 1)
        self.assertEqual(completed.record.failure_streak, 0)

    def test_source_only_progress_never_rearms_same_episode_circuit(self) -> None:
        record = None
        for index in range(2):
            wall = 1_000_000 + index * 700_000
            _, pending = self._reserve_and_mark(
                record,
                source=100 + index * 100,
                delivered=10,
                wall=wall,
                mono=10_000 + index * 700_000,
                attempt=f"attempt-{index}",
            )
            settled = self._settle_pending(
                pending,
                source=200 + index * 100,
                delivered=10,
                wall=wall + 10,
                mono=10_010 + index * 700_000,
            )
            self.assertEqual(settled.record.outcome, LedgerOutcome.NO_PROGRESS)
            record = settled.record
        self.assertTrue(record.circuit_open)

        blocked = reserve_attempt(
            record,
            repair_observation(
                observed_at=ClockStamp(3_000_000, "boot-a", 2_010_000)
            ),
            attempt_id="attempt-source-only-third",
            delivery_proof=delivery_proof(
                record.episode_key,
                now=ClockStamp(3_000_000, "boot-a", 2_010_000),
                source_frontier=10_000,
                committed_frontier=10,
            ),
            now=ClockStamp(3_000_000, "boot-a", 2_010_000),
        )
        self.assertFalse(blocked.changed)
        self.assertEqual(blocked.reason, "same_episode_circuit_open")

    def test_delivery_progress_rearms_open_same_episode(self) -> None:
        record = None
        for index in range(2):
            wall = 1_000_000 + index * 700_000
            _, pending = self._reserve_and_mark(
                record,
                source=100 + index * 100,
                wall=wall,
                mono=10_000 + index * 700_000,
                attempt=f"attempt-open-{index}",
            )
            record = self._settle_pending(
                pending,
                source=200 + index * 100,
                delivered=10,
                wall=wall + 10,
                mono=10_010 + index * 700_000,
            ).record
        self.assertTrue(record.circuit_open)

        rearmed = reserve_attempt(
            record,
            repair_observation(
                durable_anchor="anchor-11",
                observed_at=ClockStamp(3_000_000, "boot-a", 2_010_000),
                delivery_progress=True,
            ),
            attempt_id="attempt-after-delivery",
            delivery_proof=delivery_proof(
                record.episode_key,
                now=ClockStamp(3_000_000, "boot-a", 2_010_000),
                source_frontier=999,
                committed_frontier=11,
                durable_anchor="anchor-11",
            ),
            now=ClockStamp(3_000_000, "boot-a", 2_010_000),
        )
        self.assertTrue(rearmed.changed, rearmed.reason)
        self.assertEqual(rearmed.record.lifetime_no_effect_count, 0)
        self.assertEqual(rearmed.record.failure_streak, 0)
        self.assertEqual(rearmed.record.baseline_delivered, 11)

    def test_new_episode_has_independent_budget(self) -> None:
        _, pending = self._reserve_and_mark()
        old = self._settle_pending(
            pending,
            source=200,
            delivered=10,
            wall=1_000_100,
            mono=10_100,
        ).record
        successor_now = ClockStamp(1_700_000, "boot-a", 710_000)
        new = reserve_attempt(
            old,
            repair_observation(
                "ep-successor",
                durable_anchor="anchor-0",
                observed_at=successor_now,
            ),
            attempt_id="successor-attempt",
            delivery_proof=delivery_proof(
                "ep-successor",
                now=successor_now,
                source_frontier=0,
                committed_frontier=0,
                durable_anchor="anchor-0",
            ),
            now=successor_now,
        )
        self.assertTrue(new.changed)
        self.assertEqual(new.record.episode_key, "ep-successor")
        self.assertEqual(new.record.lifetime_no_effect_count, 0)
        self.assertEqual(new.record.attempts_spent, 0)

    def test_clock_rollback_future_and_monotonic_regression_fail_closed(self) -> None:
        previous = ClockStamp(10_000, "boot-a", 500)
        self.assertFalse(clock_allows_transition(previous, ClockStamp(9_999, "boot-b", 1)))
        self.assertFalse(clock_allows_transition(previous, ClockStamp(9_999, "boot-a", 501)))
        self.assertFalse(clock_allows_transition(previous, ClockStamp(10_001, "boot-a", 499)))
        self.assertTrue(clock_allows_transition(previous, ClockStamp(10_001, "boot-a", 501)))

    def test_clock_rejects_u64_overflow(self) -> None:
        maximum = (1 << 64) - 1
        previous = ClockStamp(maximum, "boot-a", maximum)
        self.assertFalse(
            clock_allows_transition(
                previous,
                ClockStamp(maximum + 1, "boot-b", maximum + 1),
            )
        )

    def test_clock_rejects_across_boot_future_beyond_injected_skew(self) -> None:
        trusted_now = ClockStamp(10_000, "boot-current", 500)
        self.assertFalse(
            clock_allows_transition(
                ClockStamp(9_000, "boot-old", 999_999),
                ClockStamp(1_000_000, "boot-new", 1),
                trusted_now=trusted_now,
                max_future_skew_ms=100,
            )
        )

    def test_clock_rejects_same_boot_future_monotonic_against_injected_now(self) -> None:
        trusted_now = ClockStamp(10_000, "boot-a", 100)
        future = ClockStamp(10_000, "boot-a", 300_101)
        self.assertFalse(
            clock_allows_transition(future, future, trusted_now=trusted_now)
        )

    def test_u64_attempt_and_failure_counters_fail_closed(self) -> None:
        reserved, pending = self._reserve_and_mark()
        exhausted = mark_effect_pending(
            replace(reserved, attempts_spent=MAX_U64),
            attempt_id=reserved.attempt_id,
            now=ClockStamp(1_000_001, "boot-a", 10_001),
        )
        self.assertFalse(exhausted.changed)
        self.assertEqual(exhausted.reason, "attempt_counter_overflow")

        now = ClockStamp(1_000_100, "boot-a", 10_100)
        overflow = settle_effect(
            replace(
                pending,
                failure_streak=MAX_U64,
                lifetime_no_effect_count=MAX_U64,
                attempts_spent=MAX_U64,
            ),
            repair_observation(observed_at=now),
            attempt_id=pending.attempt_id,
            delivery_proof=delivery_proof(
                pending.episode_key,
                now=now,
                source_frontier=200,
                committed_frontier=10,
            ),
            now=now,
        )
        self.assertFalse(overflow.changed)
        self.assertEqual(overflow.reason, "backoff_clock_overflow")

    def test_reserve_attempt_rejects_unknown_in_memory_ledger_state(self) -> None:
        reserved, _ = self._reserve_and_mark()
        unknown = replace(reserved, state="future-ledger-state")
        now = ClockStamp(1_000_100, "boot-a", 10_100)
        transition = reserve_attempt(
            unknown,
            repair_observation(
                unknown.episode_key,
                durable_anchor=unknown.durable_anchor,
                observed_at=now,
            ),
            attempt_id="attempt-after-unknown-state",
            delivery_proof=delivery_proof(
                unknown.episode_key,
                now=now,
                source_frontier=unknown.baseline_source,
                committed_frontier=unknown.baseline_delivered,
                durable_anchor=unknown.durable_anchor,
            ),
            now=now,
        )
        self.assertFalse(transition.changed)
        self.assertTrue(transition.must_reprobe)
        self.assertEqual(transition.record, unknown)
        self.assertEqual(transition.reason, "ledger_state_not_typed")

    def test_corrupt_future_and_legacy_ledger_fail_closed(self) -> None:
        for raw, reason in (
            ("{", "corrupt_json"),
            ('{"schema_version": 2}', "future_or_unknown_schema"),
            ('{"schema_version": 0}', "legacy_inconclusive"),
            ('{"schema_version": 1}', "malformed_record"),
        ):
            loaded = load_ledger_json(raw, trusted_now=self.t0)
            self.assertTrue(loaded.fail_closed)
            self.assertIsNone(loaded.record)
            self.assertEqual(loaded.reason, reason)

    def test_valid_ledger_record_loads(self) -> None:
        raw = json.dumps(
            {
                "schema_version": 1,
                "attempt_id": "attempt-valid",
                "episode_key": "episode-valid",
                "source_id": "source-valid",
                "source_generation": 1,
                "durable_anchor": "anchor-valid",
                "baseline_source": 10,
                "baseline_delivered": 5,
                "state": "settled",
                "outcome": "inconclusive",
                "failure_streak": 0,
                "lifetime_no_effect_count": 0,
                "attempts_spent": 0,
                "retry_not_before_ms": None,
                "clock_stamp": {
                    "wall_ms": 1000,
                    "boot_id": "boot-valid",
                    "monotonic_ms": 10,
                },
            }
        )
        loaded = load_ledger_json(raw, trusted_now=self.t0)
        self.assertFalse(loaded.fail_closed)
        self.assertEqual(loaded.record.episode_key, "episode-valid")

    def test_semantically_impossible_pending_ledger_fails_closed(self) -> None:
        raw = json.dumps(
            {
                "schema_version": 1,
                "attempt_id": "attempt-impossible",
                "episode_key": "episode-impossible",
                "source_id": "source-impossible",
                "source_generation": 1,
                "durable_anchor": "anchor-impossible",
                "baseline_source": 10,
                "baseline_delivered": 5,
                "state": "effect_pending",
                "outcome": "verified_delivery_progress",
                "failure_streak": 0,
                "lifetime_no_effect_count": 0,
                "attempts_spent": 0,
                "retry_not_before_ms": None,
                "clock_stamp": {
                    "wall_ms": 1000,
                    "boot_id": "boot-valid",
                    "monotonic_ms": 10,
                },
            }
        )
        loaded = load_ledger_json(raw, trusted_now=self.t0)
        self.assertTrue(loaded.fail_closed)
        self.assertIsNone(loaded.record)
        self.assertEqual(loaded.reason, "semantic_invariant_violation")

    def test_impossible_ledger_outcome_and_counter_combinations_fail_closed(self) -> None:
        base = {
            "schema_version": 1,
            "attempt_id": "attempt-invariant",
            "episode_key": "episode-invariant",
            "source_id": "source-invariant",
            "source_generation": 1,
            "durable_anchor": "anchor-invariant",
            "baseline_source": 10,
            "baseline_delivered": 5,
            "state": "settled",
            "outcome": "inconclusive",
            "failure_streak": 0,
            "lifetime_no_effect_count": 0,
            "attempts_spent": 0,
            "retry_not_before_ms": None,
            "clock_stamp": {
                "wall_ms": 1000,
                "boot_id": "boot-valid",
                "monotonic_ms": 10,
            },
        }
        mutations = {
            "pending_without_unsettled_spend": {
                "state": "effect_pending",
                "attempts_spent": 1,
                "failure_streak": 1,
                "lifetime_no_effect_count": 1,
            },
            "verified_with_failure_counts": {
                "outcome": "verified_delivery_progress",
                "attempts_spent": 1,
                "failure_streak": 1,
                "lifetime_no_effect_count": 1,
            },
            "superseded_without_unsettled_spend": {
                "outcome": "superseded",
                "attempts_spent": 1,
                "failure_streak": 1,
                "lifetime_no_effect_count": 1,
            },
            "cancelled_reservation_with_retry": {
                "retry_not_before_ms": 2000,
            },
            "failed_without_spend_or_retry": {
                "outcome": "failed",
            },
            "no_progress_retry_before_settlement": {
                "outcome": "no_progress",
                "attempts_spent": 1,
                "failure_streak": 1,
                "lifetime_no_effect_count": 1,
                "retry_not_before_ms": 999,
            },
        }
        for name, mutation in mutations.items():
            record = {**base, **mutation}
            with self.subTest(name=name):
                loaded = load_ledger_json(json.dumps(record), trusted_now=self.t0)
                self.assertTrue(loaded.fail_closed)
                self.assertIsNone(loaded.record)
                self.assertEqual(loaded.reason, "semantic_invariant_violation")

    def test_future_ledger_clock_fails_closed_against_injected_now(self) -> None:
        raw = json.dumps(
            {
                "schema_version": 1,
                "attempt_id": "attempt-future",
                "episode_key": "episode-future",
                "source_id": "source-future",
                "source_generation": 1,
                "durable_anchor": "anchor-future",
                "baseline_source": 10,
                "baseline_delivered": 5,
                "state": "settled",
                "outcome": "inconclusive",
                "failure_streak": 0,
                "lifetime_no_effect_count": 0,
                "attempts_spent": 0,
                "retry_not_before_ms": None,
                "clock_stamp": {
                    "wall_ms": self.t0.wall_ms + 300_001,
                    "boot_id": "boot-future",
                    "monotonic_ms": 1,
                },
            }
        )
        loaded = load_ledger_json(raw, trusted_now=self.t0)
        self.assertTrue(loaded.fail_closed)
        self.assertEqual(loaded.reason, "clock_fail_closed")

    def test_every_fault_fixture_executes_and_matches_expected_transition(self) -> None:
        cases = fixture_data()["fault_cases"]
        self.assertGreaterEqual(len(cases), 10)
        for case in cases:
            with self.subTest(case=case.get("name")):
                self.assertIsInstance(case.get("input"), dict)
                self.assertIsInstance(case.get("expected"), dict)
                actual = execute_fault_fixture(case)
                for key, expected in case["expected"].items():
                    self.assertIn(key, actual)
                    self.assertEqual(actual[key], expected)


class BackpressureTests(unittest.TestCase):
    def test_batch_and_frontier_reject_untyped_bounds_and_dispositions(self) -> None:
        entry = SourceRange("a", 0, 6, RangeKind.ASSISTANT_TEXT, "answer")
        with self.assertRaisesRegex(ValueError, "u64 bounds"):
            plan_bounded_batch(
                [entry],
                start_cursor=False,
                max_pending_items=1,
                max_pending_bytes=10,
            )

        plan = plan_bounded_batch(
            [entry], start_cursor=0, max_pending_items=1, max_pending_bytes=10
        )
        committed = commit_dispositions(plan, {0: ["8001"]})
        with self.assertRaisesRegex(ValueError, "unknown disposition"):
            advance_committed_frontier(
                0, [replace(committed[0], kind="future_disposition")]
            )
        with self.assertRaisesRegex(ValueError, "policy reason"):
            advance_committed_frontier(
                0, [replace(committed[0], policy_reason="arbitrary")]
            )
        with self.assertRaisesRegex(ValueError, "Discord evidence"):
            advance_committed_frontier(
                0, [replace(committed[0], discord_message_ids=())]
            )
        with self.assertRaisesRegex(ValueError, "exactly one Discord id"):
            commit_dispositions(plan, {0: ["8001", "8002"]})

    def test_commit_and_frontier_reject_arbitrary_discord_id_values(self) -> None:
        entry = SourceRange("a", 0, 6, RangeKind.ASSISTANT_TEXT, "answer")
        plan = plan_bounded_batch(
            [entry], start_cursor=0, max_pending_items=1, max_pending_bytes=10
        )
        for invalid_ids in (
            [None],
            [True],
            [{}],
            [42],
            ["not-a-snowflake"],
            ["9" * 5000],
            None,
            "8001",
        ):
            with self.subTest(invalid_ids=invalid_ids):
                with self.assertRaisesRegex(ValueError, "Discord"):
                    commit_dispositions(plan, {0: invalid_ids})

        committed = commit_dispositions(plan, {0: ["8001"]})
        for invalid_id in (None, True, {}, 42, "not-a-snowflake"):
            with self.subTest(frontier_id=invalid_id):
                with self.assertRaisesRegex(ValueError, "Discord evidence"):
                    advance_committed_frontier(
                        0,
                        [replace(committed[0], discord_message_ids=(invalid_id,))],
                    )

    def test_commit_and_frontier_require_globally_unique_discord_message_ids(
        self,
    ) -> None:
        first = SourceRange("a", 0, 3, RangeKind.ASSISTANT_TEXT, "one")
        second = SourceRange("b", 3, 6, RangeKind.ASSISTANT_TEXT, "two")
        plan = plan_bounded_batch(
            [first, second],
            start_cursor=0,
            max_pending_items=2,
            max_pending_bytes=10,
        )
        with self.assertRaisesRegex(ValueError, "globally unique"):
            commit_dispositions(plan, {0: ["8001"], 1: ["8001"]})

        committed = commit_dispositions(plan, {0: ["8001"], 1: ["8002"]})
        duplicate_at_frontier = (
            committed[0],
            replace(committed[1], discord_message_ids=("8001",)),
        )
        with self.assertRaisesRegex(ValueError, "globally unique"):
            advance_committed_frontier(0, duplicate_at_frontier)

    def test_planner_rejects_actual_duplicate_marker_without_metadata(self) -> None:
        marker = "[ADK-E2E:run:case:1:BEGIN]"
        entry = SourceRange(
            "a",
            0,
            len((marker + marker).encode("utf-8")),
            RangeKind.ASSISTANT_TEXT,
            marker + marker,
        )
        with self.assertRaisesRegex(ValueError, "duplicate E2E marker"):
            plan_bounded_batch(
                [entry],
                start_cursor=0,
                max_pending_items=1,
                max_pending_bytes=100,
            )

    def test_tool_bulk_is_summarized_with_bounded_memory_and_cursor_forward(self) -> None:
        assistant = SourceRange("a", 0, 6, RangeKind.ASSISTANT_TEXT, "answer")
        tool_one_body = "x" * 400
        marker = "[ADK-E2E:run:bulk:1:END]"
        tool_two_body = marker + "y" * (400 - len(marker))
        tool_one = SourceRange("t1", 6, 406, RangeKind.TOOL_BULK, tool_one_body)
        tool_two = SourceRange(
            "t2",
            406,
            806,
            RangeKind.TOOL_BULK,
            tool_two_body,
            marker,
        )
        plan = plan_bounded_batch(
            [assistant, tool_one, tool_two],
            start_cursor=0,
            max_pending_items=3,
            max_pending_bytes=390,
        )
        self.assertIsNone(plan.blocked_reason)
        self.assertLessEqual(plan.admitted_items, 3)
        self.assertLessEqual(plan.admitted_bytes, 390)
        self.assertEqual(
            [entry.kind for entry in plan.dispositions],
            [DispositionKind.DELIVERED, DispositionKind.SUMMARIZED],
        )
        self.assertIn(marker, plan.dispositions[1].expected_body)
        self.assertEqual(len(plan.dispositions[1].source_range_sha256s), 2)
        self.assertEqual(plan.processed_end, 806)

        committed = commit_dispositions(plan, {0: ["2001"], 1: ["2002"]})
        self.assertEqual(advance_committed_frontier(0, committed), 806)

    def test_uncommitted_summary_does_not_advance_frontier(self) -> None:
        entry = SourceRange("t", 0, 500, RangeKind.TOOL_BULK, "x" * 500)
        plan = plan_bounded_batch(
            [entry], start_cursor=0, max_pending_items=1, max_pending_bytes=300
        )
        self.assertEqual(plan.dispositions[0].kind, DispositionKind.SUMMARIZED)
        self.assertEqual(advance_committed_frontier(0, plan.dispositions), 0)

    def test_oversized_assistant_blocks_without_cursor_advance(self) -> None:
        entry = SourceRange("a", 10, 110, RangeKind.ASSISTANT_TEXT, "x" * 100)
        plan = plan_bounded_batch(
            [entry], start_cursor=10, max_pending_items=4, max_pending_bytes=50
        )
        self.assertEqual(plan.blocked_reason, "high_value_range_requires_utf8_safe_split")
        self.assertEqual(plan.processed_end, 10)
        self.assertEqual(plan.dispositions, ())

    def test_control_omission_requires_durable_disposition_before_advance(self) -> None:
        entry = SourceRange("c", 0, 4, RangeKind.CONTROL, "ctrl")
        plan = plan_bounded_batch(
            [entry], start_cursor=0, max_pending_items=1, max_pending_bytes=10
        )
        self.assertEqual(plan.dispositions[0].kind, DispositionKind.OMITTED_POLICY)
        self.assertEqual(advance_committed_frontier(0, plan.dispositions), 0)
        committed = commit_dispositions(plan, {})
        self.assertEqual(advance_committed_frontier(0, committed), 4)

    def test_oversized_tool_group_admits_bounded_prefix_and_explicit_suffix(self) -> None:
        ranges = []
        cursor = 0
        for sequence in range(1, 9):
            marker = f"[ADK-E2E:run:bulk-prefix:{sequence}:END]"
            entry = SourceRange(
                f"t{sequence}",
                cursor,
                cursor + len(marker),
                RangeKind.TOOL_BULK,
                marker,
                marker,
            )
            ranges.append(entry)
            cursor = entry.end
        plan = plan_bounded_batch(
            ranges,
            start_cursor=0,
            max_pending_items=1,
            max_pending_bytes=240,
        )
        self.assertEqual(len(plan.dispositions), 1)
        self.assertEqual(plan.dispositions[0].kind, DispositionKind.SUMMARIZED)
        self.assertGreater(plan.processed_end, 0)
        self.assertLess(plan.processed_end, cursor)
        self.assertEqual(plan.blocked_reason, "bounded_tool_bulk_suffix")

        remaining = [entry for entry in ranges if entry.start >= plan.processed_end]
        next_plan = plan_bounded_batch(
            remaining,
            start_cursor=plan.processed_end,
            max_pending_items=1,
            max_pending_bytes=240,
        )
        self.assertGreater(next_plan.processed_end, plan.processed_end)

    def test_frontier_rejects_duplicate_or_overlapping_committed_spans(self) -> None:
        first = SourceRange("one", 0, 3, RangeKind.ASSISTANT_TEXT, "one")
        plan = plan_bounded_batch(
            [first], start_cursor=0, max_pending_items=1, max_pending_bytes=10
        )
        committed = commit_dispositions(plan, {0: ["7001"]})
        with self.assertRaisesRegex(ValueError, "duplicate"):
            advance_committed_frontier(0, [committed[0], committed[0]])


class EvidenceOracleTests(unittest.TestCase):
    def setUp(self) -> None:
        data = fixture_data()
        self.good = data["oracle_good_bundle"]
        self.pins = data["oracle_pinned_inputs"]

    def validate(self, bundle: dict) -> list[str]:
        return validate_evidence_bundle(
            bundle,
            pinned_manifest=self.pins["pinned_manifest"],
            actual_source_id=self.pins["actual_source_id"],
            actual_source_bytes=self.pins["actual_source_utf8"].encode("utf-8"),
            discord_channel_id=self.pins["discord_channel_id"],
            discord_after_message_id=self.pins["discord_after_message_id"],
        )

    def test_good_bundle_reconciles_source_dispositions_discord_and_frontier(self) -> None:
        self.assertEqual(self.validate(self.good), [])

    def test_missing_discord_message_is_rejected(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["discord_observations"] = [
            item for item in broken["discord_observations"] if item["message_id"] != "1002"
        ]
        self.assertIn("discord_message_not_observed", self.validate(broken))

    def test_duplicate_discord_message_id_is_rejected(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["dispositions"][1]["discord_message_ids"] = ["1001"]
        self.assertIn("discord_message_reused", self.validate(broken))

    def test_noncanonical_numeric_alias_cannot_evade_duplicate_id_rejection(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["dispositions"][1]["discord_message_ids"] = ["01001"]
        broken["discord_observations"][1]["message_id"] = "01001"
        errors = self.validate(broken)
        self.assertIn("discord_message_id_invalid", errors)
        self.assertNotEqual(errors, [])

    def test_unhashable_and_non_string_disposition_message_ids_fail_closed(self) -> None:
        for invalid_id in (
            {},
            [],
            None,
            True,
            4254,
            "not-a-snowflake",
            "9" * 5000,
        ):
            broken = copy.deepcopy(self.good)
            broken["dispositions"][0]["discord_message_ids"] = [invalid_id]
            with self.subTest(invalid_id=invalid_id):
                self.assertIn("discord_message_id_invalid", self.validate(broken))

        broken = copy.deepcopy(self.good)
        broken["dispositions"][0]["discord_message_ids"] = None
        self.assertIn("disposition_message_ids_invalid", self.validate(broken))

    def test_body_hash_mismatch_is_rejected(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["discord_observations"][0]["normalized_body"] += "mutant"
        self.assertIn("discord_body_hash_mismatch", self.validate(broken))

    def test_wrong_bot_author_is_rejected(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["discord_observations"][0]["author_id"] = "999"
        self.assertIn("discord_message_author_mismatch", self.validate(broken))

    def test_source_disposition_hash_mismatch_is_rejected(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["dispositions"][0]["source_sha256"] = "0" * 64
        self.assertIn("disposition_source_hash_mismatch", self.validate(broken))

        broken = copy.deepcopy(self.good)
        broken["dispositions"][0]["source_range_sha256s"] = ["0" * 64]
        self.assertIn("disposition_source_hashes_mismatch", self.validate(broken))

    def test_policy_version_mismatch_is_rejected(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["dispositions"][1]["policy_reason"] = "other-policy:tool_bulk_overflow"
        self.assertIn("disposition_policy_mismatch", self.validate(broken))

    def test_unreferenced_duplicate_marker_is_rejected(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["discord_observations"].append(
            {
                "message_id": "1004",
                "author_id": "424254",
                "observed_at": "2026-07-13T10:00:03Z",
                "edited_at": None,
                "revision_index": 0,
                "message_role": "relay_body",
                "normalized_body": "duplicate [ADK-E2E:run4254:high-volume:1:BEGIN]",
            }
        )
        errors = self.validate(broken)
        self.assertIn(
            "marker_count:[ADK-E2E:run4254:high-volume:1:BEGIN]:2", errors
        )

    def test_actual_source_duplicate_marker_ignores_nullable_metadata(self) -> None:
        marker = "[ADK-E2E:run4254:high-volume:1:BEGIN]"
        actual_source = (marker + marker).encode("utf-8")

        def length_prefixed_hash(raw: bytes) -> str:
            digest = hashlib.sha256()
            digest.update(len(raw).to_bytes(8, "big"))
            digest.update(raw)
            return digest.hexdigest()

        source_hash = length_prefixed_hash(actual_source)
        broken = copy.deepcopy(self.good)
        broken["manifest"]["source_end"] = len(actual_source)
        pins = copy.deepcopy(broken["manifest"])
        broken["source_ranges"] = [
            {
                "range_id": "duplicate-marker-range",
                "start": 0,
                "end": len(actual_source),
                "kind": "assistant_text",
                "sha256": source_hash,
                "marker": None,
            }
        ]
        broken["dispositions"] = [
            {
                "range_ids": ["duplicate-marker-range"],
                "kind": "delivered",
                "policy_reason": "assistant_text_must_deliver",
                "source_sha256": source_hash,
                "source_range_sha256s": [source_hash],
                "discord_message_ids": ["1001"],
                "expected_body_sha256": source_hash,
                "committed": True,
            }
        ]
        body = actual_source.decode("utf-8")
        broken["discord_observations"] = [
            {
                "message_id": "1001",
                "channel_id": broken["manifest"]["channel_id"],
                "author_id": broken["manifest"]["bot_author_id"],
                "observed_at": "2026-07-13T10:00:01Z",
                "edited_at": None,
                "revision_index": 0,
                "message_role": "relay_body",
                "raw_body": body,
                "normalized_body": body,
            }
        ]
        broken["delivery_observation"]["committed_frontier"] = len(actual_source)
        errors = validate_evidence_bundle(
            broken,
            pinned_manifest=pins,
            actual_source_id=self.pins["actual_source_id"],
            actual_source_bytes=actual_source,
            discord_channel_id=self.pins["discord_channel_id"],
            discord_after_message_id=self.pins["discord_after_message_id"],
        )
        self.assertIn("source_marker_duplicate", errors)
        self.assertIn("source_marker_metadata_missing", errors)

    def test_multi_range_planner_serialization_is_accepted_by_oracle(self) -> None:
        actual_source = self.pins["actual_source_utf8"]
        marker = self.good["source_ranges"][1]["marker"]
        tool_ranges = [
            SourceRange(
                "r2",
                47,
                109,
                RangeKind.TOOL_BULK,
                actual_source[47:109],
                marker,
            ),
            SourceRange(
                "r3",
                109,
                123,
                RangeKind.TOOL_BULK,
                actual_source[109:123],
            ),
        ]
        plan = plan_bounded_batch(
            tool_ranges,
            start_cursor=47,
            max_pending_items=1,
            max_pending_bytes=400,
        )
        self.assertEqual(plan.dispositions[0].range_ids, ("r2", "r3"))
        committed = commit_dispositions(plan, {0: ["1002"]})
        serialized = serialize_planned_disposition(committed[0])
        self.assertEqual(len(serialized["source_range_sha256s"]), 2)

        bundle = copy.deepcopy(self.good)
        bundle["source_ranges"][2]["kind"] = "tool_bulk"
        bundle["dispositions"] = [bundle["dispositions"][0], serialized]
        body = committed[0].expected_body
        bundle["discord_observations"][1]["raw_body"] = body
        bundle["discord_observations"][1]["normalized_body"] = body
        self.assertEqual(self.validate(bundle), [])

    def test_digest_marker_summary_uses_same_canonical_body_hash_in_oracle(
        self,
    ) -> None:
        ranges = []
        cursor = 0
        for sequence in range(1, 5):
            marker = f"[ADK-E2E:digest-run:digest-case:{sequence}:END]"
            entry = SourceRange(
                f"tool-{sequence}",
                cursor,
                cursor + len(marker.encode("utf-8")),
                RangeKind.TOOL_BULK,
                marker,
                marker,
            )
            ranges.append(entry)
            cursor = entry.end

        plan = plan_bounded_batch(
            ranges,
            start_cursor=0,
            max_pending_items=1,
            max_pending_bytes=250,
        )
        self.assertIsNone(plan.blocked_reason)
        self.assertEqual(len(plan.dispositions), 1)
        self.assertIn("marker_count=4", plan.dispositions[0].expected_body)
        self.assertIn("marker_sha256=", plan.dispositions[0].expected_body)
        committed = commit_dispositions(plan, {0: ["9001"]})
        serialized = serialize_planned_disposition(committed[0])

        source_bytes = "".join(entry.body for entry in ranges).encode("utf-8")
        manifest = {
            "schema_version": 1,
            "run_id": "digest-run",
            "case_id": "digest-case",
            "provider": "claude",
            "channel_id": "4254",
            "base_sha": "a" * 40,
            "binary_sha": "b" * 40,
            "policy_version": "relay-target-v1",
            "normalization_version": "discord-body-v1",
            "actual_source_id": "source-digest",
            "source_generation": 7,
            "coordinate_space": "raw_utf8_bytes",
            "source_start": 0,
            "source_end": len(source_bytes),
            "durable_frontier_before": 0,
            "durable_anchor_before": "anchor-before",
            "durable_anchor_after": "anchor-after",
            "discord_complete": True,
            "bot_author_id": "424254",
            "discord_after_message_id": "9000",
            "started_at": "2026-07-13T10:00:00Z",
        }
        bundle = {
            "manifest": manifest,
            "source_ranges": [
                {
                    "range_id": entry.range_id,
                    "start": entry.start,
                    "end": entry.end,
                    "kind": entry.kind.value,
                    "sha256": committed[0].source_range_sha256s[index],
                    "marker": entry.marker,
                    "marker_in_source": True,
                }
                for index, entry in enumerate(ranges)
            ],
            "dispositions": [serialized],
            "discord_observations": [
                {
                    "message_id": "9001",
                    "channel_id": "4254",
                    "author_id": "424254",
                    "observed_at": "2026-07-13T10:00:01Z",
                    "edited_at": None,
                    "revision_index": 0,
                    "message_role": "relay_body",
                    "raw_body": committed[0].expected_body,
                    "normalized_body": committed[0].expected_body,
                }
            ],
            "delivery_observation": {
                "source_id": "source-digest",
                "source_generation": 7,
                "durable_anchor": "anchor-after",
                "committed_frontier": len(source_bytes),
            },
            "unrelated_sessions": [
                {
                    "provider": "claude",
                    "channel_id": "4255",
                    "identity_complete": True,
                    "observed_at": "2026-07-13T10:00:02Z",
                    "relay_gap": False,
                    "regression": False,
                },
                {
                    "provider": "codex",
                    "channel_id": "4256",
                    "identity_complete": True,
                    "observed_at": "2026-07-13T10:00:03Z",
                    "relay_gap": False,
                    "regression": False,
                },
            ],
        }
        self.assertEqual(
            validate_evidence_bundle(
                bundle,
                pinned_manifest=manifest,
                actual_source_id="source-digest",
                actual_source_bytes=source_bytes,
                discord_channel_id="4254",
                discord_after_message_id="9000",
            ),
            [],
        )

    def test_source_gap_is_rejected(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["source_ranges"][1]["start"] += 1
        self.assertIn("source_range_gap_or_order", self.validate(broken))

    def test_unwitnessed_source_marker_is_rejected(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["source_ranges"][0]["marker_in_source"] = False
        self.assertIn("source_marker_not_witnessed", self.validate(broken))

    def test_discord_revision_gap_is_rejected(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["discord_observations"].append(
            {
                "message_id": "1001",
                "author_id": "424254",
                "observed_at": "2026-07-13T10:00:04Z",
                "edited_at": "2026-07-13T10:00:04Z",
                "revision_index": 2,
                "message_role": "relay_body",
                "normalized_body": "edited",
            }
        )
        self.assertIn("discord_revision_gap", self.validate(broken))

    def test_frontier_ahead_is_rejected(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["delivery_observation"]["committed_frontier"] += 1
        self.assertIn("durable_frontier_mismatch", self.validate(broken))

    def test_pending_disposition_leaves_failed_suffix(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["dispositions"][1]["committed"] = False
        errors = self.validate(broken)
        self.assertIn("durable_frontier_mismatch", errors)
        self.assertIn("pending_source_suffix", errors)

    def test_unrelated_session_regression_is_rejected(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["unrelated_sessions"][0]["relay_gap"] = True
        self.assertIn("unrelated_session_regression", self.validate(broken))

    def test_self_declared_source_hashes_are_not_authoritative(self) -> None:
        broken = copy.deepcopy(self.good)
        for source_range, disposition in zip(
            broken["source_ranges"], broken["dispositions"]
        ):
            source_range["sha256"] = "0" * 64
            disposition["source_sha256"] = "0" * 64
        broken["actual_source_artifact"] = {
            "source_id": broken["manifest"]["actual_source_id"],
            "raw_utf8": "tampered bytes that do not match the claims",
        }
        self.assertIn("source_range_hash_mismatch", self.validate(broken))

    def test_self_consistent_wrong_marker_is_rejected_from_actual_source(self) -> None:
        broken = copy.deepcopy(self.good)
        wrong = "[ADK-E2E:run4254:high-volume:99:BEGIN]"
        broken["source_ranges"][0]["marker"] = wrong
        broken["discord_observations"][0]["normalized_body"] = wrong + "answer-one"
        broken["dispositions"][0]["expected_body_sha256"] = normalized_body_sha256(
            [broken["discord_observations"][0]["normalized_body"]]
        )
        self.assertIn("source_marker_bytes_mismatch", self.validate(broken))

    def test_unknown_disposition_kind_fails_closed(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["dispositions"][2]["kind"] = "future_kind"
        broken["dispositions"][2]["policy_reason"] = "arbitrary"
        self.assertIn("disposition_kind_unknown", self.validate(broken))

    def test_arbitrary_omission_reason_fails_closed(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["dispositions"][2]["policy_reason"] = "relay-target-v1:anything"
        self.assertIn("disposition_policy_mismatch", self.validate(broken))

    def test_missing_durable_anchor_fails_closed(self) -> None:
        broken = copy.deepcopy(self.good)
        del broken["delivery_observation"]["durable_anchor"]
        self.assertIn("durable_anchor_missing", self.validate(broken))

    def test_empty_or_single_provider_unrelated_evidence_fails_closed(self) -> None:
        empty = copy.deepcopy(self.good)
        empty["unrelated_sessions"] = []
        self.assertIn("unrelated_provider_evidence_missing", self.validate(empty))
        single = copy.deepcopy(self.good)
        single["unrelated_sessions"] = single["unrelated_sessions"][:1]
        self.assertIn(
            "unrelated_provider_evidence_missing", self.validate(single)
        )

    def test_empty_source_coordinate_span_fails_closed(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["source_ranges"][1]["end"] = 47
        broken["source_ranges"][2]["start"] = 47
        self.assertIn("source_range_empty_or_invalid", self.validate(broken))

    def test_manifest_must_match_external_pin(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["manifest"]["base_sha"] = "0" * 40
        self.assertIn("manifest_not_externally_pinned", self.validate(broken))

    def test_actual_source_extent_is_external_and_required(self) -> None:
        errors = validate_evidence_bundle(
            self.good,
            pinned_manifest=self.pins["pinned_manifest"],
            actual_source_id=self.pins["actual_source_id"],
            actual_source_bytes=b"self-declared bundle cannot replace actual source",
            discord_channel_id=self.pins["discord_channel_id"],
            discord_after_message_id=self.pins["discord_after_message_id"],
        )
        self.assertIn("actual_source_extent_mismatch", errors)

    def test_discord_channel_and_cutoff_are_externally_scoped(self) -> None:
        wrong_channel = copy.deepcopy(self.good)
        wrong_channel["discord_observations"][0]["channel_id"] = "other-channel"
        self.assertIn("discord_message_channel_mismatch", self.validate(wrong_channel))
        before_cutoff = copy.deepcopy(self.good)
        before_cutoff["discord_observations"][0]["message_id"] = "998"
        before_cutoff["dispositions"][0]["discord_message_ids"] = ["998"]
        self.assertIn("discord_message_outside_cutoff", self.validate(before_cutoff))

    def test_duplicate_committed_span_fails_closed(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["dispositions"].append(copy.deepcopy(broken["dispositions"][0]))
        self.assertIn(
            "duplicate_or_overlapping_committed_span", self.validate(broken)
        )

    def test_reordered_disposition_ranges_fail_closed(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["dispositions"][1]["range_ids"] = ["r2", "r1"]
        self.assertIn(
            "disposition_ranges_noncontiguous_or_reordered", self.validate(broken)
        )

    def test_unknown_source_kind_fails_closed(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["source_ranges"][1]["kind"] = "future_kind"
        self.assertIn("source_range_kind_unknown", self.validate(broken))

    def test_durable_anchor_must_match_external_pin(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["delivery_observation"]["durable_anchor"] = "self-declared-anchor"
        self.assertIn("durable_anchor_not_externally_pinned", self.validate(broken))

    def test_non_string_marker_fails_closed_without_oracle_exception(self) -> None:
        broken = copy.deepcopy(self.good)
        broken["source_ranges"][0]["marker"] = 4254
        errors = self.validate(broken)
        self.assertIn("source_marker_invalid", errors)
        self.assertIn("source_marker_bytes_mismatch", errors)

    def test_unsupported_disposition_kind_range_pair_has_no_null_reason_escape(self) -> None:
        broken = copy.deepcopy(self.good)
        control = broken["dispositions"][2]
        control["kind"] = "delivered"
        control["policy_reason"] = None
        self.assertIn("disposition_policy_mismatch", self.validate(broken))

    def test_manifest_frontier_and_completion_types_are_strict(self) -> None:
        invalid_frontier = copy.deepcopy(self.good)
        invalid_frontier["manifest"]["durable_frontier_before"] = "0"
        self.assertIn(
            "manifest_durable_frontier_invalid", self.validate(invalid_frontier)
        )

        invalid_completion = copy.deepcopy(self.good)
        invalid_completion["manifest"]["discord_complete"] = "yes"
        self.assertIn("discord_fetch_incomplete", self.validate(invalid_completion))

    def test_even_externally_pinned_manifest_schema_fails_closed(self) -> None:
        for field, value, expected in (
            ("provider", ["claude"], "manifest_provider_unknown"),
            (
                "normalization_version",
                "future-normalization",
                "normalization_version_unknown",
            ),
            ("started_at", 4254, "manifest_text_identity_invalid"),
        ):
            bundle = copy.deepcopy(self.good)
            pins = copy.deepcopy(self.pins["pinned_manifest"])
            bundle["manifest"][field] = value
            pins[field] = value
            with self.subTest(field=field):
                errors = validate_evidence_bundle(
                    bundle,
                    pinned_manifest=pins,
                    actual_source_id=self.pins["actual_source_id"],
                    actual_source_bytes=self.pins["actual_source_utf8"].encode(
                        "utf-8"
                    ),
                    discord_channel_id=self.pins["discord_channel_id"],
                    discord_after_message_id=self.pins["discord_after_message_id"],
                )
                self.assertIn(expected, errors)


if __name__ == "__main__":
    unittest.main()

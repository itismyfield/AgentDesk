"""Independent checks that memory samples bracket the real build steps."""

import copy
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[1]
BUILD = "Archive, list, compare and plan"
PHASES = {
    "rust_build": [BUILD],
    "base_compare": ["Compare tests with the comparison base"],
    "pg15_owner_proof": ["PostgreSQL 15 exact owner proof"],
    "lint": ["just lint"],
    "scripts": ["Run script checks"],
    "relay_authority_mutations": ["Require relay-authority mutations to be killed"],
}
NIGHTLY_RELAY = "relay_authority_mutations_full"
TARGETS = {"ci-pr.yml": set(PHASES), "ci-main.yml": {"rust_build", "lint", "scripts"}, "ci-nightly.yml": {"rust_build", NIGHTLY_RELAY}}
PHASES[NIGHTLY_RELAY] = PHASES["relay_authority_mutations"]
NAMES = ["Start runner memory sampler", "Begin runner memory step", "End runner memory step", "Summarize runner memory", "Upload runner memory"]
COMMANDS = dict(zip(NAMES[:4], ["start", 'begin "$MEMORY_STEP"', "end", "finish"]))
CHECKPOINT = ["Save runner memory checkpoint", "Upload runner memory checkpoint"]
COMMANDS[CHECKPOINT[0]] = "checkpoint"
ARTIFACT = {
    "name": "runner-memory-${{ github.job }}-${{ strategy.job-index || 0 }}-attempt-${{ github.run_attempt }}",
    "path": "${{ runner.temp }}/runner-memory", "if-no-files-found": "warn", "retention-days": 7,
}


class RunnerMemoryWiring(unittest.TestCase):
    def assert_job(self, workflow, job_id, job):
        steps = job["steps"]
        names = [s.get("name") for s in steps]
        self.assertTrue(steps[0]["uses"].startswith("actions/checkout@"))
        self.assertEqual(names[1], NAMES[0])
        self.assertEqual(names[-2:], NAMES[-2:])
        if job_id in ("lint", "pg15_owner_proof"):
            saved, uploaded = [names.index(name) for name in CHECKPOINT]
            self.assertEqual(saved + 1, uploaded)
            first = "just lint" if job_id == "lint" else "Reuse archived lib test binary"
            self.assertLess(uploaded, names.index(first))
            self.assertEqual(names.count(CHECKPOINT[0]), 1)
            self.assertEqual(names.count(CHECKPOINT[1]), 1)
        else:
            self.assertFalse(set(CHECKPOINT) & set(names))
        phases = PHASES[job_id] + (["Non-lib tests and doctests"] if workflow == "ci-main.yml" and job_id == "lint" else [])
        begins = [i for i, s in enumerate(steps) if s.get("name") == NAMES[1]]
        ends = [i for i, s in enumerate(steps) if s.get("name") == NAMES[2]]
        self.assertEqual([steps[i]["env"]["MEMORY_STEP"] for i in begins], phases)
        self.assertEqual(len(ends), len(phases))
        for name in (NAMES[0], *NAMES[-2:]):
            self.assertEqual(names.count(name), 1)
        for label, begin, end in zip(phases, begins, ends):
            first = "Protect writer gate aggregate wiring (#5308)" if workflow == "ci-pr.yml" and job_id == "scripts" else label
            last = "Production PR cap" if workflow == "ci-pr.yml" and job_id == "scripts" else label
            self.assertEqual(begin + 1, names.index(first))
            self.assertEqual(end - 1, names.index(last))
            build = steps[names.index(label)]
            self.assertNotIn("continue-on-error", build)
            self.assertEqual(steps[begin].get("if"), build.get("if"))
        for step in steps:
            name = step.get("name")
            if "runner-memory.sh" in step.get("run", ""):
                self.assertIn(name, COMMANDS)
            if name not in NAMES + CHECKPOINT:
                continue
            self.assertIs(step.get("continue-on-error"), True)
            if name in NAMES[2:]:
                self.assertEqual(step.get("if"), "always()")
            if name in COMMANDS:
                self.assertEqual(step["run"], "bash scripts/ci/runner-memory.sh " + COMMANDS[name])
                self.assertEqual(step.get("shell"), "bash")
                self.assertEqual(step["env"].get("BASH_ENV"), "/dev/null")
            else:
                self.assertEqual(step.get("uses"), "actions/upload-artifact@v4")
                expected_artifact = ARTIFACT
                if name == CHECKPOINT[1]:
                    expected_artifact = {**ARTIFACT, "name": ARTIFACT["name"] + "-before-build",
                                         "path": "${{ runner.temp }}/runner-memory-checkpoint"}
                self.assertEqual(step["with"], expected_artifact)
                self.assertEqual(step.get("timeout-minutes"), 2)
        self.assertEqual(steps[-2].get("timeout-minutes"), 1)

    def test_every_heavy_build_has_bounded_step_measurements(self):
        for workflow, targets in TARGETS.items():
            jobs = yaml.safe_load((ROOT / ".github/workflows" / workflow).read_text())["jobs"]
            self.assertEqual({j for j, job in jobs.items() if any(s.get("name") in NAMES for s in job.get("steps", []))}, targets)
            for job_id in targets:
                with self.subTest(workflow=workflow, job=job_id):
                    self.assert_job(workflow, job_id, jobs[job_id])

    def test_relay_mutation_jobs_name_mid_build_copies_after_their_final_artifact(self):
        for workflow, job_id in (("ci-pr.yml", "relay_authority_mutations"), ("ci-nightly.yml", NIGHTLY_RELAY)):
            job = yaml.safe_load((ROOT / ".github/workflows" / workflow).read_text())["jobs"][job_id]
            with self.subTest(workflow=workflow):
                self.assertEqual(job["env"]["RUNNER_MEMORY_EXPORT_NAME"], ARTIFACT["name"])
                # Dense copies near the 61-63 minute loss bound how long the runner stayed alive.
                self.assertEqual(job["env"]["RUNNER_MEMORY_EXPORT_MINUTES"], "15 30 45 50 55 58 60")
                gate = next(step for step in job["steps"] if step.get("name") == PHASES[job_id][0])
                self.assertLess(60, gate["timeout-minutes"])
                # The exporter reads the artifact token that Setup sccache leaves in the environment.
                names = [step.get("name") for step in job["steps"]]
                self.assertLess(names.index("Setup sccache"), names.index(NAMES[1]))

    def test_cleanup_condition_and_phase_label_are_required(self):
        source = yaml.safe_load((ROOT / ".github/workflows/ci-pr.yml").read_text())["jobs"]["rust_build"]
        for name, field, replacement in [(NAMES[2], "if", "success()"), (NAMES[1], "env", {"BASH_ENV": "/dev/null", "MEMORY_STEP": "setup"})]:
            job = copy.deepcopy(source)
            next(s for s in job["steps"] if s.get("name") == name)[field] = replacement
            with self.subTest(mutation=name), self.assertRaises(AssertionError):
                self.assert_job("ci-pr.yml", "rust_build", job)

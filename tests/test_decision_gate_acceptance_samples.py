"""Keep fixed regex and candidate-word shortcuts below the acceptance threshold."""

import hashlib
import json
from pathlib import Path
import re
import unittest


FIXTURE = Path(__file__).parent / "fixtures/decision_gate/acceptance_samples.json"
NORMAL_PATH = re.compile(r"정상|무관|관계없이|보존")
WORDS = re.compile(r"[a-z0-9_]+|[가-힣]+")
CONTRACT_SHA256 = "b3a696a62e6dbdbf20a661b7a40ea6c17ad23fc41bafd7bcf77d3bc7ed092f87"
MAX_SHORTCUT_ACCURACY = 0.75
OVERLAP_THRESHOLD = 0.20


def input_text(sample):
    return sample["title"] + " " + sample["background"]


def tokens(text):
    return set(WORDS.findall(text.lower()))


def regex_prediction(sample):
    return "new" if NORMAL_PATH.search(sample["background"]) else "duplicate"


def overlap_prediction(sample, fixture):
    query = tokens(input_text(sample))
    scores = []
    for issue in fixture["issue_sets"][sample["issue_set"]]:
        candidate = tokens(issue["title"])
        scores.append(len(query & candidate) / len(query | candidate))
    return "duplicate" if max(scores) >= OVERLAP_THRESHOLD else "new"


class AcceptanceSampleLeakTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.fixture = json.loads(FIXTURE.read_text(encoding="utf-8"))

    def assert_shortcut_bounded(self, name, predictions):
        samples = self.fixture["samples"]
        correct = sum(
            prediction == sample["expected"]
            for prediction, sample in zip(predictions, samples, strict=True)
        )
        accuracy = correct / len(samples)
        print(f"{name}: correct={correct}/{len(samples)} accuracy={accuracy:.6f}")
        self.assertIn("duplicate", predictions)
        self.assertIn("new", predictions)
        self.assertLessEqual(accuracy, MAX_SHORTCUT_ACCURACY)

    def test_review_normal_path_regex_is_not_a_label_shortcut(self):
        self.assert_shortcut_bounded(
            "normal_path_regex",
            [regex_prediction(sample) for sample in self.fixture["samples"]],
        )

    def test_candidate_word_overlap_is_not_a_label_shortcut(self):
        self.assert_shortcut_bounded(
            "candidate_word_jaccard_ge_0.20",
            [
                overlap_prediction(sample, self.fixture)
                for sample in self.fixture["samples"]
            ],
        )

    def test_labels_ids_keys_and_candidate_sets_are_unchanged(self):
        contract = {
            "issue_sets": self.fixture["issue_sets"],
            "labels": [
                {
                    field: sample[field]
                    for field in ("sample_id", "expected", "key", "issue_set")
                }
                for sample in self.fixture["samples"]
            ],
        }
        serialized = json.dumps(
            contract, ensure_ascii=False, sort_keys=True, separators=(",", ":")
        ).encode("utf-8")
        self.assertEqual(hashlib.sha256(serialized).hexdigest(), CONTRACT_SHA256)


if __name__ == "__main__":
    unittest.main(verbosity=2)

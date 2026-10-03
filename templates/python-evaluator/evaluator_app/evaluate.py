"""Customize this file for your evaluation logic; return a score, never policy."""


class Evaluator:
    def __init__(self, artifact):
        # Replace with your validated model loading. No request-time downloads.
        if set(artifact) != {"markers"} or not isinstance(artifact["markers"], list):
            raise ValueError("artifact contract")
        if not 1 <= len(artifact["markers"]) <= 32:
            raise ValueError("marker count")
        if any(
            not isinstance(x, str) or not x or len(x.encode()) > 256
            for x in artifact["markers"]
        ):
            raise ValueError("marker bound")
        self.markers = tuple(artifact["markers"])

    def score(self, text, joined):
        # Replace with NLP/ML inference. Convert library scalars to Python float.
        return float(any(marker in text or marker in joined for marker in self.markers))

    def self_test(self):
        # Replace with your model's representative known-answer readiness test.
        assert self.score(self.markers[0], "") == 1.0
        assert self.score("", "") == 0.0

"""Regression tests for load validation, paired tails and aligned CPU windows."""
import copy
import sys
import unittest
from pathlib import Path

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
from screen_gate import gate, validate_load, SCENARIOS
from screen_cpu import measurement


class ScreenMeasurements(unittest.TestCase):
    def load(self):
        spec = SCENARIOS["unconstrained"][1]
        return {"screen": {"spec": {"source": spec, "viewers": 5, "shares": 1, "bitrate": 8_000_000, "fps": 60, "keyframe": 200000, "via": "none"},
                           "steady_viewer_mbps": [7.8] * 5,
                           "samples": [{"unix_s": 12}, {"unix_s": 20}]}}, spec

    def test_gate_requires_expected_load_and_every_viewer(self):
        meta, spec = self.load()
        validate_load(meta, spec, False)
        for invalid in ({}, {"screen": None}, {"screen": {"spec": {"source": "wrong"}}}):
            with self.assertRaises(RuntimeError):
                validate_load(invalid, spec, False)
        for rates in ([0] * 5, [7.8] * 4, [7.8, 7.8, 0, 7.8, 7.8], [float("nan")] * 5):
            invalid = copy.deepcopy(meta)
            invalid["screen"]["steady_viewer_mbps"] = rates
            with self.assertRaises(RuntimeError):
                validate_load(invalid, spec, False)

    def test_downlink_requires_one_share_still_flowing(self):
        spec = SCENARIOS["downlink"][1]
        meta = {"screen": {"spec": {"source": spec, "viewers": 1, "shares": 2, "bitrate": 3_000_000, "fps": 30, "keyframe": 150000, "via": "viewers"},
                           "steady_viewer_mbps": [2.8], "samples": [{"unix_s": 12}, {"unix_s": 20}]}}
        validate_load(meta, spec, True)
        meta["screen"]["steady_viewer_mbps"] = [0.1]
        with self.assertRaises(RuntimeError):
            validate_load(meta, spec, True)

    def test_four_starved_pairs_cannot_hide_behind_the_median(self):
        base = {"median_ms": 45, "p95_ms": 45, "p99_ms": 45, "underrun_fraction": 0, "underruns": 0}
        pairs = [{"baseline": dict(base), "screen": dict(base)} for _ in range(10)]
        for pair in pairs[:4]:
            pair["screen"]["p99_ms"] += 140
        result = gate(pairs, True)
        self.assertEqual(result["median_delta"]["p99_ms"], 0)
        self.assertEqual(result["tail_pairs"], 4)
        self.assertFalse(result["pass"])
        for pair in pairs[1:4]:
            pair["screen"]["p99_ms"] = 45
        self.assertTrue(gate(pairs, True)["pass"])
        self.assertFalse(gate(pairs[:2], True)["pass"])

    def test_cpu_and_throughput_use_the_same_settled_snapshot_interval(self):
        cpu = [(t + 0.25, (t + 0.25) * 0.1) for t in range(20)]
        screen = {"settled_unix_s": 12.5, "pauses": 0,
                  "samples": [{"unix_s": t / 2, "received_bytes": max(0, t / 2 - 12.5) * 250_000}
                              for t in range(24, 40)]}
        result = measurement(cpu, screen, 1, 45)
        self.assertEqual(result["window_start_unix_s"], 12.5)
        self.assertEqual(result["window_end_unix_s"], 19)
        self.assertAlmostEqual(result["forwarded_mbps"], 2)
        self.assertAlmostEqual(result["cpu_percent"], 10)
        limited = measurement(cpu, screen, 1, 45, seconds=3)
        self.assertEqual(limited["window_s"], 3)
        self.assertAlmostEqual(limited["forwarded_mbps"], 2)


if __name__ == "__main__":
    unittest.main()

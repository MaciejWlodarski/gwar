"""Regression tests for load validation, paired tails and aligned CPU windows."""
import contextlib
import copy
import csv
import io
import json
import os
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
import screen_gate
from screen_gate import gate, relay_timing, validate_load, SCENARIOS
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


class ScreenGateRuns(unittest.TestCase):
    def run_gate(self, out, scenarios, *, null=True, regress=False, unexpected_load=False):
        calls = []

        def client(binary, path, markers, env):
            calls.append((path.parent.name, path.stem, env.copy()))
            is_second_arm = path.stem.endswith("screen")
            latency = 59.6 if regress and is_second_arm else 45.0
            meta = {
                "markers": 1, "n": 1, "lost_markers": [], "marker_identity_count": 8,
                "matching_window_ms": 2000, "minimum_latency_ms": 5, "extra_delay_ms": 0,
                "marker_frequencies_hz": list(range(8)), "marker_interval_samples": 12000,
                "sample_rate": 48000, "warmup_samples": 720000, "rejected_onsets": [],
                "marker_captures": [{"identity": 0, "capture_s": 1}],
                "detected_onsets": [{"identity": 0, "sample": 48000, "playout_s": 1 + latency / 1000}],
                "median_ms": latency, "p95_ms": latency, "p99_ms": latency,
                "receiver_frames_received": 100,
                "receiver_playout": {"underruns": int(regress and is_second_arm),
                                     "underrun_samples": 223 if regress and is_second_arm else 0,
                                     "slots": [{"concealments": 0, "late_drops": 0}]},
                "screen": {} if unexpected_load else None,
            }
            with path.open("x", newline="") as file:
                writer = csv.writer(file)
                writer.writerow(["marker", "identity", "latency_ms", "capture_s", "playout_s", "playout_sample"])
                writer.writerow([0, 0, latency, 1, 1 + latency / 1000, 48000])
            Path(f"{path}.json").write_text(json.dumps(meta))
            return 123

        argv = ["screen_gate.py", "--binary", sys.executable, "--pairs", "2", "--markers", "1", "--out", str(out)]
        if null:
            argv.append("--null")
        for scenario in scenarios:
            argv.extend(["--scenario", scenario])
        with mock.patch.object(sys, "argv", argv), mock.patch.object(screen_gate, "run_client", side_effect=client), \
                mock.patch.object(screen_gate, "install_signals"), contextlib.redirect_stdout(io.StringIO()), \
                mock.patch.dict(os.environ, {"LATENCY_SCREEN": "inherited load", "LATENCY_NET": "inherited net"}):
            screen_gate.main()
        return calls

    def test_null_runs_both_arms_without_load_with_paired_seeds_and_alternation(self):
        with tempfile.TemporaryDirectory(prefix="gwar-gate-test-") as tmp:
            out = Path(tmp) / "out"
            scenarios = ["unconstrained", "unconstrained-loss", "downlink"]
            calls = self.run_gate(out, scenarios)
            self.assertEqual(len(calls), 12)
            for index, scenario in enumerate(scenarios):
                runs = calls[index * 4:index * 4 + 4]
                names = ["run-01-baseline", "run-02-screen", "run-03-screen", "run-04-baseline"]
                self.assertEqual([name for _, name, _ in runs], names)
                net = SCENARIOS[scenario][0]
                for run, (name, _, env) in enumerate(runs):
                    self.assertEqual(name, scenario)
                    self.assertNotIn("LATENCY_SCREEN", env)
                    self.assertEqual(env.get("LATENCY_NET"), f"seed={run // 2 + 1};{net}" if net else None)
                    self.assertEqual(env["LATENCY_WAIT_CONNECTED"], "1")
                    self.assertEqual(env["LATENCY_WARMUP_MS"], "15000")
                manifest = json.loads((out / scenario / "manifest.json").read_text())
                self.assertTrue(manifest["null"])
                self.assertIsNone(manifest["screen"])
                self.assertEqual(manifest["order"], [name.rsplit("-", 1)[1] for name in names])
            summary = json.loads((out / "summary.json").read_text())
            self.assertTrue(summary["null"])
            self.assertTrue(summary["pass"])
            for name, scenario in summary["scenarios"].items():
                self.assertIsNone(scenario["screen"])
                self.assertEqual(scenario["constrained"], SCENARIOS[name][2])
                for pair in scenario["pairs"]:
                    for arm in ("baseline", "screen"):
                        self.assertIsNone(pair[arm]["screen"])
                        self.assertIsNone(pair[arm]["load_check"])

    def test_null_keeps_latency_underrun_and_pair_rules(self):
        with tempfile.TemporaryDirectory(prefix="gwar-gate-test-") as tmp:
            out = Path(tmp) / "out"
            with self.assertRaisesRegex(SystemExit, "gate missed: unconstrained"):
                self.run_gate(out, ["unconstrained"], regress=True)
            scenario = json.loads((out / "summary.json").read_text())["scenarios"]["unconstrained"]
            self.assertFalse(scenario["checks"]["p95 within 2.0 ms"])
            self.assertFalse(scenario["checks"]["p99 within 2.0 ms"])
            self.assertFalse(scenario["checks"]["no extra underruns"])
            self.assertEqual(scenario["tail_pairs"], 2)
            self.assertEqual(scenario["allowed_tail_pairs"], 0)
            self.assertFalse(scenario["pass"])

    def test_null_rejects_an_unexpected_load(self):
        with tempfile.TemporaryDirectory(prefix="gwar-gate-test-") as tmp:
            with self.assertRaisesRegex(RuntimeError, "unexpected screen load in A/A"):
                self.run_gate(Path(tmp) / "out", ["unconstrained"], unexpected_load=True)

    def test_default_still_requires_screen_load(self):
        with tempfile.TemporaryDirectory(prefix="gwar-gate-test-") as tmp:
            with self.assertRaisesRegex(RuntimeError, "missing or unexpected screen load spec"):
                self.run_gate(Path(tmp) / "out", ["unconstrained"], null=False)

    def test_relay_timing_explains_the_direct_path_and_keeps_relay_measurements(self):
        direct = relay_timing({"network": None})
        self.assertEqual(direct["status"], "not_applicable")
        self.assertIn("disabled", direct["reason"])
        timing = {"queue_max_ms": 12, "scheduling_lateness_max_ms": 3, "scheduling_late_over_5ms": 0}
        measured = relay_timing({"network": {"up": timing, "down": timing}})
        self.assertEqual(measured, {"status": "measured", "up": timing, "down": timing})


if __name__ == "__main__":
    unittest.main()

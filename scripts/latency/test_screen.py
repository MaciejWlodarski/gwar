"""Regression tests for interleaved A/A calibration, load validation and CPU windows."""
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
from screen_gate import gate, relay_timing, runtime_estimate, validate_load, SCENARIOS
from screen_cpu import measurement


def pairs(deltas, *, underruns=None, fractions=None):
    """Synthetic per-pair measurements; negative deltas are deliberately preserved."""
    values = []
    for index, delta in enumerate(deltas):
        a, b = underruns[index] if underruns else (0, 0)
        base = {"median_ms": 60, "p95_ms": 60, "p99_ms": 60,
                "underrun_fraction": 0, "underruns": a}
        second = {**base, "p95_ms": 60 + delta, "p99_ms": 60 + delta, "underruns": b,
                  "underrun_fraction": fractions[index] if fractions else 0}
        values.append({"baseline": base, "screen": second})
    return values


class CalibratedGate(unittest.TestCase):
    def test_noisy_aa_passes_instead_of_failing_absolute_rules(self):
        # Similar scale to the supplied VM A/A: median +5.5 ms, six tails, 3 -> 8 underruns.
        noisy = pairs([-37, -20, -14, -2, 4, 7, 15, 20, 30, 41],
                      underruns=[(1, 0)] * 3 + [(0, 0)] + [(0, 1)] * 4 + [(0, 2)] * 2)
        result = gate(noisy, copy.deepcopy(noisy), False)
        self.assertEqual(result["sets"]["aa"]["median_delta"]["p99_ms"], 5.5)
        self.assertEqual(result["sets"]["aa"]["tail_pairs"], 6)
        self.assertEqual(result["sets"]["aa"]["underrun_excess"], 5)
        self.assertTrue(result["pass"])
        worse = copy.deepcopy(noisy)
        for pair in worse:
            pair["screen"]["p99_ms"] += 3
        regression = gate(noisy, worse, False)
        self.assertFalse(regression["checks"]["p99_ms"])
        self.assertFalse(regression["pass"])

    def test_latency_tolerances_include_equality(self):
        aa = pairs([0] * 10)
        for constrained, tolerance in ((False, 2), (True, 20)):
            with self.subTest(constrained=constrained):
                self.assertTrue(gate(aa, pairs([tolerance] * 10), constrained)["pass"])
                self.assertFalse(gate(aa, pairs([tolerance + 0.001] * 10), constrained)["pass"])
        p95_only = pairs([0] * 10)
        for pair in p95_only:
            pair["screen"]["p95_ms"] += 3
        self.assertFalse(gate(aa, p95_only, False)["checks"]["p95_ms"])
        self.assertTrue(gate(aa, p95_only, True)["pass"])

    def test_four_starved_pairs_cannot_hide_behind_the_median(self):
        aa = pairs([0] * 10)
        screen = pairs([140] * 4 + [0] * 6)
        result = gate(aa, screen, True)
        self.assertEqual(result["sets"]["screen"]["median_delta"]["p99_ms"], 0)
        self.assertEqual(result["sets"]["screen"]["tail_pairs"], 4)
        self.assertFalse(result["pass"])
        one_tail = pairs([140] + [0] * 9)
        self.assertTrue(gate(aa, one_tail, True)["pass"])
        self.assertFalse(gate(aa[:2], one_tail[:2], True)["pass"])

    def test_tail_allowance_is_added_to_calibration(self):
        aa = pairs([140] * 2 + [0] * 8)
        screen = pairs([140] * 3 + [0] * 7)
        result = gate(aa, screen, True)
        self.assertEqual(result["comparison"]["tail_pairs"]["limit"], 3)
        self.assertTrue(result["pass"])
        self.assertFalse(gate(aa, pairs([140] * 4 + [0] * 6), True)["checks"]["tail_pairs"])
        # Eight pairs allow no additional tail above A/A.
        self.assertFalse(gate(aa[:8], screen[:8], True)["checks"]["tail_pairs"])

    def test_underrun_excess_is_signed_and_has_no_extra_tolerance(self):
        aa = pairs([0] * 10, underruns=[(2, 0)] + [(0, 0)] * 9)
        screen = pairs([0] * 10, underruns=[(1, 0)] + [(0, 0)] * 9)
        result = gate(aa, screen, False)
        self.assertEqual(result["comparison"]["underrun_excess"]["limit"], -2)
        self.assertFalse(result["checks"]["underrun_excess"])
        quiet = pairs([0] * 10)
        one_underrun = pairs([0] * 10, underruns=[(0, 1)] + [(0, 0)] * 9)
        result = gate(quiet, one_underrun, False)
        self.assertTrue(result["checks"]["tail_pairs"])
        self.assertFalse(result["checks"]["underrun_excess"])
        self.assertTrue(gate(aa, aa, False)["pass"])

    def test_constrained_underrun_fraction_uses_half_a_percentage_point(self):
        aa = pairs([0] * 10, fractions=[0.001] * 10)
        result = gate(aa, pairs([0] * 10, fractions=[0.006] * 10), True)
        self.assertAlmostEqual(result["comparison"]["underrun_fraction"]["limit"], 0.006)
        self.assertTrue(result["checks"]["underrun_fraction"])
        # The fraction margin does not waive the separate absolute-threshold tail count.
        self.assertFalse(result["checks"]["tail_pairs"])
        noisy = pairs([0] * 10, fractions=[0.006] * 10)
        self.assertTrue(gate(noisy, pairs([0] * 10, fractions=[0.011] * 10), True)["pass"])
        self.assertFalse(gate(aa, pairs([0] * 10, fractions=[0.006001] * 10), True)["pass"])

    def test_negative_calibration_is_not_clipped(self):
        result = gate(pairs([-10] * 10), pairs([0] * 10), False)
        self.assertEqual(result["comparison"]["p99_ms"]["limit"], -8)
        self.assertFalse(result["pass"])

    def test_missing_or_unequal_calibration_is_rejected(self):
        for aa, screen in (([], []), (pairs([0]), []), (pairs([0]), pairs([0, 0]))):
            with self.assertRaisesRegex(ValueError, "equal, nonempty"):
                gate(aa, screen, False)

    def test_runtime_budget_counts_four_runs_per_block(self):
        estimate = runtime_estimate(6, 8, 100)
        self.assertEqual(estimate["runs"], 192)
        self.assertEqual(estimate["measurement_seconds_per_run"], 43)
        self.assertAlmostEqual(estimate["estimated_hours"], 2.56)
        self.assertAlmostEqual(runtime_estimate(6, 10, 100)["estimated_hours"], 3.2)


class ScreenMeasurements(unittest.TestCase):
    def load(self):
        spec = SCENARIOS["unconstrained"][1]
        return {"screen": {"spec": {"source": spec, "viewers": 5, "shares": 1, "bitrate": 8_000_000, "fps": 60, "keyframe": 200000, "via": "none"},
                           "steady_viewer_mbps": [7.8] * 5, "pauses": 0,
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

    def test_unconstrained_pauses_or_missing_pause_count_fail_even_at_full_rate(self):
        meta, spec = self.load()
        for pause_count in (1, None):
            meta["screen"]["pauses"] = pause_count
            with self.assertRaisesRegex(RuntimeError, "zero pauses"):
                validate_load(meta, spec, False)

    def test_downlink_requires_one_share_still_flowing(self):
        spec = SCENARIOS["downlink"][1]
        meta = {"screen": {"spec": {"source": spec, "viewers": 1, "shares": 2, "bitrate": 3_000_000, "fps": 30, "keyframe": 150000, "via": "viewers"},
                           "pauses": 1, "steady_viewer_mbps": [2.8], "samples": [{"unix_s": 12}, {"unix_s": 20}]}}
        validate_load(meta, spec, True)
        meta["screen"]["steady_viewer_mbps"] = [0.1]
        with self.assertRaises(RuntimeError):
            validate_load(meta, spec, True)

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
    def run_gate(self, out, scenarios, *, null=False, regress=False, unexpected_load=False,
                 missing_load=False, load_failure=False, legacy=False):
        calls = []

        def client(binary, path, markers, env):
            set_name = path.parent.name
            calls.append((path.parent.parent.name, set_name, path.stem, env.copy()))
            is_second_arm = path.stem.endswith("screen")
            regressed = regress and set_name == "screen" and is_second_arm
            latency = 59.6 if regressed else 45.0
            load = None
            if "LATENCY_SCREEN" in env and not missing_load:
                if load_failure:
                    raise RuntimeError("screen load task stopped unexpectedly")
                source = env["LATENCY_SCREEN"]
                spec = dict(option.split("=") for option in source.split(","))
                spec["bitrate"] = int(float(spec["bitrate"].removesuffix("mbit")) * 1e6)
                for key in ("shares", "fps", "keyframe", "viewers"):
                    spec[key] = int(spec[key])
                load = {"spec": {"source": source, **spec}, "pauses": 0,
                        "steady_viewer_mbps": [spec["bitrate"] / 1e6] * spec["viewers"],
                        "samples": [{"unix_s": 12}, {"unix_s": 20}]}
            if unexpected_load:
                load = {}
            meta = {
                "markers": 1, "n": 1, "lost_markers": [], "marker_identity_count": 8,
                "matching_window_ms": 2000, "minimum_latency_ms": 5, "extra_delay_ms": 0,
                "marker_frequencies_hz": list(range(8)), "marker_interval_samples": 12000,
                "sample_rate": 48000, "warmup_samples": 720000, "rejected_onsets": [],
                "marker_captures": [{"identity": 0, "capture_s": 1}],
                "detected_onsets": [{"identity": 0, "sample": 48000, "playout_s": 1 + latency / 1000}],
                "median_ms": latency, "p95_ms": latency, "p99_ms": latency,
                "receiver_frames_received": 100,
                "receiver_playout": {"underruns": int(regressed), "underrun_samples": 223 if regressed else 0,
                                     "slots": [{"concealments": 0, "late_drops": 0}]},
                "screen": load,
            }
            if not legacy:
                meta["receiver_device"] = {"late_over_5ms": 0, "max_busy_ms": 0.05}
                meta["receiver_record"] = {"capacity_samples": 3_744_000, "recorded_samples": 2_100_000,
                                           "overflow_samples": 0}
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
                mock.patch.dict(os.environ, {"LATENCY_SCREEN": "inherited load", "LATENCY_NET": "inherited net",
                                            "LATENCY_URL": "inherited remote", "LATENCY_SERVER": "inherited address"}):
            screen_gate.main()
        return calls

    def test_sets_are_interleaved_with_paired_seeds_balanced_arms_and_validated_load(self):
        with tempfile.TemporaryDirectory(prefix="gwar-gate-test-") as tmp:
            out = Path(tmp) / "out"
            scenarios = ["unconstrained", "unconstrained-loss", "downlink"]
            calls = self.run_gate(out, scenarios)
            self.assertEqual(len(calls), 24)
            order = [("aa", "baseline"), ("aa", "screen"), ("screen", "baseline"), ("screen", "screen"),
                     ("screen", "screen"), ("screen", "baseline"), ("aa", "screen"), ("aa", "baseline")]
            for index, scenario in enumerate(scenarios):
                runs = calls[index * 8:index * 8 + 8]
                self.assertEqual([(s, name.rsplit("-", 1)[1]) for _, s, name, _ in runs], order)
                net, spec, _ = SCENARIOS[scenario]
                for run, (name, set_name, arm, env) in enumerate(runs):
                    self.assertEqual(name, scenario)
                    loaded = set_name == "screen" and arm.endswith("screen")
                    self.assertEqual(env.get("LATENCY_SCREEN"), spec if loaded else None)
                    self.assertNotIn("LATENCY_URL", env)
                    self.assertNotIn("LATENCY_SERVER", env)
                    self.assertEqual(env.get("LATENCY_NET"), f"seed={run // 4 + 1};{net}" if net else None)
                    self.assertEqual(env["LATENCY_WAIT_CONNECTED"], "1")
                    self.assertEqual(env["LATENCY_WARMUP_MS"], "15000")
                for set_name in ("aa", "screen"):
                    manifest = json.loads((out / scenario / set_name / "manifest.json").read_text())
                    self.assertEqual(manifest["set_name"], set_name)
                    self.assertEqual(manifest["screen"], spec if set_name == "screen" else None)
                    self.assertEqual(manifest["order"], ["baseline", "screen", "screen", "baseline"])
                    self.assertEqual([r["seed"] for r in manifest["runs"]], [1, 1, 2, 2])
            summary = json.loads((out / "summary.json").read_text())
            self.assertFalse(summary["null"])
            self.assertTrue(summary["pass"])
            for name, scenario in summary["scenarios"].items():
                self.assertEqual(scenario["constrained"], SCENARIOS[name][2])
                self.assertTrue(scenario["pass"])
                self.assertEqual(len(scenario["execution_order"]), 8)
                for set_name, values in scenario["sets"].items():
                    self.assertEqual(len(values["pairs"]), 2)
                    self.assertEqual(values["underrun_excess"], 0)
                    for pair in values["pairs"]:
                        self.assertIsNone(pair["baseline"]["screen"])
                        self.assertEqual(pair["screen"]["load_check"] is not None, set_name == "screen")

    def test_runs_without_device_diagnostics_still_gate_and_report_them_unavailable(self):
        # Metadata from binaries before receiver_device/receiver_record (archived runs included).
        for legacy in (True, False):
            with self.subTest(legacy=legacy), tempfile.TemporaryDirectory(prefix="gwar-gate-test-") as tmp:
                out = Path(tmp) / "out"
                self.run_gate(out, ["unconstrained"], legacy=legacy)
                summary = json.loads((out / "summary.json").read_text())
                self.assertTrue(summary["pass"])
                pair = summary["scenarios"]["unconstrained"]["sets"]["aa"]["pairs"][0]["baseline"]
                if legacy:
                    self.assertEqual(pair["device_timing"], "unavailable")
                    self.assertIsNone(pair["device_late_over_5ms"])
                    self.assertIsNone(pair["record_overflow_samples"])
                else:
                    self.assertEqual(pair["device_timing"], "available")
                    self.assertEqual((pair["device_late_over_5ms"], pair["record_overflow_samples"]), (0, 0))

    def test_null_runs_two_interleaved_aa_sets_without_load(self):
        with tempfile.TemporaryDirectory(prefix="gwar-gate-test-") as tmp:
            out = Path(tmp) / "out"
            calls = self.run_gate(out, ["unconstrained-loss"], null=True)
            self.assertEqual(len(calls), 8)
            self.assertTrue(all("LATENCY_SCREEN" not in env for _, _, _, env in calls))
            summary = json.loads((out / "summary.json").read_text())
            self.assertTrue(summary["null"])
            self.assertTrue(summary["pass"])
            self.assertIsNone(summary["scenarios"]["unconstrained-loss"]["screen"])

    def test_screen_regression_fails_and_keeps_both_sets_in_summary(self):
        with tempfile.TemporaryDirectory(prefix="gwar-gate-test-") as tmp:
            out = Path(tmp) / "out"
            with self.assertRaisesRegex(SystemExit, "gate missed: unconstrained"):
                self.run_gate(out, ["unconstrained"], regress=True)
            summary = json.loads((out / "summary.json").read_text())
            scenario = summary["scenarios"]["unconstrained"]
            self.assertFalse(summary["pass"])
            self.assertFalse(scenario["checks"]["p95_ms"])
            self.assertFalse(scenario["checks"]["p99_ms"])
            self.assertFalse(scenario["checks"]["underrun_excess"])
            self.assertFalse(scenario["checks"]["tail_pairs"])
            self.assertEqual(scenario["sets"]["aa"]["tail_pairs"], 0)
            self.assertEqual(scenario["sets"]["screen"]["tail_pairs"], 2)

    def test_no_load_arms_reject_unexpected_load(self):
        for null in (False, True):
            with self.subTest(null=null), tempfile.TemporaryDirectory(prefix="gwar-gate-test-") as tmp:
                with self.assertRaisesRegex(RuntimeError, "unexpected screen load in no-load arm"):
                    self.run_gate(Path(tmp) / "out", ["unconstrained"], null=null, unexpected_load=True)

    def test_missing_or_failed_screen_load_fails_closed(self):
        for options, error in (({"missing_load": True}, "missing or unexpected screen load spec"),
                               ({"load_failure": True}, "screen load task stopped unexpectedly")):
            with self.subTest(options=options), tempfile.TemporaryDirectory(prefix="gwar-gate-test-") as tmp:
                out = Path(tmp) / "out"
                with self.assertRaisesRegex(RuntimeError, error):
                    self.run_gate(out, ["unconstrained"], **options)
                summary = json.loads((out / "summary.json").read_text())
                self.assertFalse(summary["pass"])
                self.assertEqual(summary["scenarios"]["unconstrained"]["error"], error)
                for set_name in ("aa", "screen"):
                    manifest = json.loads((out / "unconstrained" / set_name / "manifest.json").read_text())
                    self.assertEqual(manifest["error"], error)

    def test_relay_timing_explains_the_direct_path_and_keeps_relay_measurements(self):
        direct = relay_timing({"network": None})
        self.assertEqual(direct["status"], "not_applicable")
        self.assertIn("disabled", direct["reason"])
        timing = {"queue_max_ms": 12, "scheduling_lateness_max_ms": 3, "scheduling_late_over_5ms": 0}
        measured = relay_timing({"network": {"up": timing, "down": timing}})
        self.assertEqual(measured, {"status": "measured", "up": timing, "down": timing})


if __name__ == "__main__":
    unittest.main()

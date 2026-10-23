#!/usr/bin/env python3
"""Regression checks for CI coverage selection and the required aggregate check."""
import importlib.util
from pathlib import Path
import unittest


def module(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(f"{name}.py"))
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


select = module("changes").select
check = module("check-results").check


class Coverage(unittest.TestCase):
    def test_documentation(self):
        self.assertEqual(select(["README.md", "docs/e2e.md", "docs/screenshot.png"]),
                         {"native": False, "app": False})

    def test_web_keeps_all_app_coverage(self):
        self.assertEqual(select(["apps/web/src/App.tsx", "docs/e2e.md"]),
                         {"native": False, "app": True})

    def test_shared_and_unknown_inputs_fan_out(self):
        for path in ["crates/gwar-proto/src/lib.rs", "apps/web/src/proto/ServerEvent.ts",
                     "Cargo.lock", "pnpm-lock.yaml", "apps/web/package.json",
                     ".github/workflows/ci.yml", "scripts/ci/changes.py",
                     "docs/release-key.pub", "new-component/source.xyz", ".cargo/config.toml"]:
            with self.subTest(path=path):
                self.assertEqual(select([path]), {"native": True, "app": True})

    def test_main_always_full(self):
        self.assertEqual(select(["docs/e2e.md"], full=True), {"native": True, "app": True})

    def test_renamed_source_cannot_be_hidden_as_docs(self):
        self.assertEqual(select(["crates/gwar-server/src/main.rs", "docs/old-main.md"]),
                         {"native": True, "app": True})

    @staticmethod
    def results(native, app):
        result = {"changes": {"result": "success", "outputs": {"native": native, "app": app}}}
        for name in ["rust", "rust-voice", "teamspeak"]:
            result[name] = {"result": "success" if native == "true" else "skipped"}
        for name in ["audit", "web", "desktop", "mls-wasm", "denoise-wasm", "native-build",
                     "browser-mls", "browser-smoke", "browser-voice", "browser-screen", "browser-private"]:
            result[name] = {"result": "success" if app == "true" else "skipped"}
        return result

    def test_intentional_skips_and_full_run_succeed(self):
        for native, app in [("false", "false"), ("false", "true"), ("true", "true")]:
            check(self.results(native, app))

    def test_failed_canceled_and_accidentally_skipped_jobs_fail(self):
        for status in ["failure", "cancelled", "skipped"]:
            needs = self.results("true", "true")
            needs["browser-screen"]["result"] = status
            with self.assertRaises(ValueError):
                check(needs)

    def test_failed_or_missing_selection_fails(self):
        needs = self.results("false", "false")
        needs["changes"]["result"] = "failure"
        with self.assertRaises(ValueError):
            check(needs)
        needs["changes"] = {"result": "success", "outputs": {}}
        with self.assertRaises(ValueError):
            check(needs)


if __name__ == "__main__":
    unittest.main()

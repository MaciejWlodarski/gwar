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
rewrite_sources = module("apt-sources").rewrite


class Coverage(unittest.TestCase):
    def test_ubuntu_mirrors_cover_runner_mirror_lists_and_fallback(self):
        fixture = """Types: deb
URIs: mirror+file:/etc/apt/apt-mirrors.txt
Suites: noble noble-updates noble-backports
Components: main restricted universe multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg

Types: deb
URIs: mirror+file:/etc/apt/apt-mirrors-security.txt
Suites: noble-security
Components: main restricted universe multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
"""
        for mirror in ["https://archive.ubuntu.com/ubuntu", "https://mirrors.edge.kernel.org/ubuntu"]:
            fixture, count = rewrite_sources(fixture, mirror)
            self.assertEqual(count, 2)
            self.assertEqual(fixture.count(f"URIs: {mirror}\n"), 2)
            self.assertNotIn("mirror+file:", fixture)
            self.assertIn("Suites: noble-security", fixture)
            self.assertIn("Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg", fixture)

    def test_legacy_azure_sources_and_third_party_repositories(self):
        fixture = ("deb http://azure.archive.ubuntu.com/ubuntu/ noble main\n"
                   "deb http://security.ubuntu.com/ubuntu noble-security main\n"
                   "deb https://packages.microsoft.com/ubuntu/24.04/prod noble main\n")
        text, count = rewrite_sources(fixture, "https://archive.ubuntu.com/ubuntu")
        self.assertEqual(count, 2)
        self.assertNotIn("azure", text)
        self.assertNotIn("security.ubuntu.com", text)
        self.assertIn("https://packages.microsoft.com/ubuntu/24.04/prod", text)

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

    def test_media_keeps_native_and_cross_platform_coverage(self):
        for path in ["crates/gwar-media/src/lib.rs", "crates/gwar-media/Cargo.toml",
                     "crates/gwar-media/tests/contracts.rs", "crates/gwar-media/README.md"]:
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

"""Resolve real Cargo graphs; never build or start the Driver.

Requires the pinned Rust toolchain and cached Cargo dependencies. Run with
`python3 -m unittest discover -s libs/cua-driver/scripts/tests -p test_ffi_features.py`.
CUA_FFI_TEST_TARGET may select another target for dependency-only validation.
"""

import os
import subprocess
import unittest
from pathlib import Path


WORKSPACE = Path(__file__).resolve().parents[2] / "rust"


class FfiFeaturesTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        version = subprocess.run(
            ["rustc", "-vV"], cwd=WORKSPACE,
            check=True, capture_output=True, text=True, timeout=30,
        ).stdout
        host = next(
            line.removeprefix("host: ")
            for line in version.splitlines()
            if line.startswith("host: ")
        )
        cls.target = os.environ.get("CUA_FFI_TEST_TARGET", host)

    def packages(self, *args):
        result = subprocess.run(
            [
                "cargo", "tree", "--locked", "--offline",
                "--manifest-path", str(WORKSPACE / "Cargo.toml"),
                "--target", self.target, "--edges", "normal,build,dev",
                "--prefix", "none", "--format", "{p}", *args,
            ],
            cwd=WORKSPACE, capture_output=True, text=True, timeout=120,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        packages = {line.split()[0] for line in result.stdout.splitlines() if line.strip()}
        self.assertTrue(packages, "Cargo returned an empty graph")
        return packages

    def assert_no_uniffi(self, packages):
        self.assertFalse(
            sorted(name for name in packages if name.startswith("uniffi")),
            "UniFFI re-entered the native/test dependency graph",
        )

    def test_native_release_roots_do_not_enable_ffi(self):
        packages = self.packages("-p", "cua-driver", "-p", "cursor-theme-cli")
        self.assertTrue({"cua-driver-sdk", "cua-driver-contract"} <= packages)
        self.assert_no_uniffi(packages)

    def test_rust_sdk_without_defaults_does_not_enable_ffi(self):
        self.assert_no_uniffi(self.packages(
            "-p", "cua-driver-sdk", "--no-default-features"
        ))

    def test_rust_contract_without_defaults_does_not_enable_ffi(self):
        self.assert_no_uniffi(self.packages(
            "-p", "cua-driver-contract", "--no-default-features"
        ))

    def test_existing_sdk_and_contract_defaults_retain_bindings(self):
        for package in ("cua-driver-sdk", "cua-driver-contract"):
            with self.subTest(package=package):
                self.assertIn("uniffi", self.packages("-p", package))

    def test_explicit_sdk_ffi_feature_retains_bindings(self):
        self.assertIn("uniffi", self.packages(
            "-p", "cua-driver-sdk", "--no-default-features", "--features", "ffi"
        ))


if __name__ == "__main__":
    unittest.main()

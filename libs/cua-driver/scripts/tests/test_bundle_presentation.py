"""Validate development bundle metadata without building or installing an app."""

import plistlib
import unittest
from pathlib import Path


DRIVER_ROOT = Path(__file__).resolve().parents[2]
BUNDLE = DRIVER_ROOT / "rust/scripts/CuaDriverBundle"


class BundlePresentationTest(unittest.TestCase):
    def setUp(self):
        with (BUNDLE / "Contents/Info.plist").open("rb") as reader:
            self.info = plistlib.load(reader)

    def test_display_name_and_permission_copy(self):
        for key in ("CFBundleName", "CFBundleDisplayName"):
            self.assertEqual(self.info[key], "cua")
        for key in ("NSScreenCaptureUsageDescription", "NSAppleEventsUsageDescription"):
            self.assertTrue(self.info[key].startswith("cua "))

    def test_no_custom_icon_in_template(self):
        self.assertFalse(any(key.startswith("CFBundleIcon") for key in self.info))
        files = {
            path.relative_to(BUNDLE).as_posix()
            for path in BUNDLE.rglob("*") if path.is_file()
        }
        self.assertEqual(files, {"Contents/Info.plist", "Contents/MacOS/.gitkeep"})

    def test_runtime_identity_and_capabilities_are_unchanged(self):
        expected = {
            "CFBundleIdentifier": "com.trycua.driver",
            "CFBundleExecutable": "cua-driver",
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": "0.0.0-dev",
            "CFBundleVersion": "0",
            "LSMinimumSystemVersion": "13.0",
            "LSUIElement": True,
            "NSHighResolutionCapable": True,
            "NSSupportsAutomaticTermination": True,
        }
        for key, value in expected.items():
            with self.subTest(key=key):
                self.assertEqual(self.info[key], value)

    def test_local_installer_retains_local_identity_and_display_name(self):
        source = (DRIVER_ROOT / "scripts/_install-local-rust.sh").read_text()
        for key, value in {
            "CFBundleName": "cua",
            "CFBundleDisplayName": "cua",
            "CFBundleIdentifier": "com.trycua.driver.local",
            "CFBundleExecutable": "cua-driver-local",
        }.items():
            self.assertIn(f'plutil -replace {key} -string "{value}"', source)
        self.assertIn('APP_DEST="/Applications/CuaDriverLocal.app"', source)


if __name__ == "__main__":
    unittest.main()

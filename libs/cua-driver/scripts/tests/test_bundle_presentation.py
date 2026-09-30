"""Validate development bundle metadata without building or installing an app."""

import plistlib
import unittest
from pathlib import Path


DRIVER_ROOT = Path(__file__).resolve().parents[2]
BUNDLE = DRIVER_ROOT / "rust/scripts/CuaDriverBundle"
ENTITLEMENTS = DRIVER_ROOT / "rust/scripts/CuaDriver.entitlements"


class BundlePresentationTest(unittest.TestCase):
    def setUp(self):
        with (BUNDLE / "Contents/Info.plist").open("rb") as reader:
            self.info = plistlib.load(reader)

    def test_display_name_and_permission_copy(self):
        for key in ("CFBundleName", "CFBundleDisplayName"):
            self.assertEqual(self.info[key], "Computer Use")
        for key in ("NSScreenCaptureUsageDescription", "NSAppleEventsUsageDescription"):
            self.assertTrue(self.info[key].startswith("Computer Use "))

    def test_no_custom_icon_in_template(self):
        self.assertFalse(any(key.startswith("CFBundleIcon") for key in self.info))
        files = {
            path.relative_to(BUNDLE).as_posix()
            for path in BUNDLE.rglob("*") if path.is_file()
        }
        self.assertEqual(files, {"Contents/Info.plist", "Contents/MacOS/.gitkeep"})

    def test_runtime_identity_and_capabilities_match_muse_code(self):
        expected = {
            "CFBundleIdentifier": "com.meta.musecode.cua.driver",
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
            "CFBundleIdentifier": "com.meta.musecode.cua.driver.local",
            "CFBundleExecutable": "cua-driver-local",
        }.items():
            self.assertIn(f'plutil -replace {key} -string "{value}"', source)
        self.assertIn('APP_DEST="/Applications/MuseCodeCuaDriverLocal.app"', source)

    def test_production_history_entitlements_match_muse_code_identity(self):
        with ENTITLEMENTS.open("rb") as reader:
            entitlements = plistlib.load(reader)
        application_identifier = "4W5TH4RKQ2.com.meta.musecode.cua.driver"
        self.assertEqual(
            entitlements["com.apple.application-identifier"],
            application_identifier,
        )
        self.assertEqual(
            entitlements["keychain-access-groups"],
            [application_identifier],
        )


if __name__ == "__main__":
    unittest.main()

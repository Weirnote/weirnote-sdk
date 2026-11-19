"""Exercise helper setup from an installed wheel with a temporary app directory."""

from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import plistlib
import subprocess
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from weirnote import Client
from weirnote import _helper


class BundledHelperTests(unittest.TestCase):
    def test_concurrent_setup_reuses_one_helper_and_repairs_an_invalid_bundle(self):
        with TemporaryDirectory() as directory, patch.object(_helper, "_install_root", return_value=Path(directory)):
            def prepare(_: int) -> tuple[Path, int]:
                with _helper.bundled_executable() as executable:
                    return executable, executable.stat().st_ino

            with ThreadPoolExecutor(max_workers=2) as worker:
                first, second = worker.map(prepare, range(2))
            self.assertEqual(first, second)
            executable, inode = first
            self.assertEqual(prepare(0), (executable, inode))

            info = executable.parent.parent / "Info.plist"
            info.write_bytes(b"Invalid property list")
            self.assertNotEqual(prepare(0)[1], inode)
            self.assertEqual(prepare(0)[0], executable)

    def test_newer_helper_is_retained_when_it_passes_the_signer_requirement(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            resources = root / "resources"
            resources.mkdir()
            original_resources = _helper._RESOURCES
            metadata = json.loads((original_resources / "account-helper.json").read_text())
            # The fixture uses an ad-hoc signature. The identifier requirement
            # stands in for the same-signer requirement across release versions.
            metadata["requirement"] = f'identifier "{metadata["bundle_id"]}"'
            (resources / "account-helper.json").write_text(json.dumps(metadata))
            (resources / "account-helper.zip").symlink_to(original_resources / "account-helper.zip")
            with patch.object(_helper, "_RESOURCES", resources), patch.object(
                _helper, "_install_root", return_value=root / "apps"
            ):
                with _helper.bundled_executable() as executable:
                    bundle = executable.parents[2]
                info = bundle / "Contents/Info.plist"
                plist = plistlib.loads(info.read_bytes())
                plist["CFBundleVersion"] = "999.0.0"
                info.write_bytes(plistlib.dumps(plist))
                subprocess.run(["/usr/bin/codesign", "--force", "--sign", "-", bundle], check=True,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                with _helper.bundled_executable():
                    self.assertEqual(plistlib.loads(info.read_bytes())["CFBundleVersion"], "999.0.0")

    @unittest.skipUnless(os.getenv("WEIRNOTE_TEST_HELPER_ROOT"),
                         "enable on a desktop session with Launch Services")
    def test_managed_helper_registers_its_exact_callback(self):
        root = Path(os.environ["WEIRNOTE_TEST_HELPER_ROOT"])
        with patch.object(_helper, "_install_root", return_value=root):
            client = Client(origin="https://fixture.example")
            registered = client._invoke("register-callback")
            self.assertEqual(registered, client._invoke("check-callback"))
            self.assertTrue(registered["redirect"].endswith(".login:/oauth/callback"))


if __name__ == "__main__":
    unittest.main()

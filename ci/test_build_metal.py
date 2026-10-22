"""Offline tests for the Metal release lookup."""

import importlib.util
import json
import subprocess
import unittest
from pathlib import Path
from unittest.mock import patch


spec = importlib.util.spec_from_file_location(
    "build_metal", Path(__file__).with_name("build-metal.py")
)
build_metal = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build_metal)

ASSET = "metal-macos.tar.zst"
DIGEST = "a" * 64


def release(draft):
    return {
        "tag_name": "metal-16A242d",
        "draft": draft,
        "assets": [
            {"name": ASSET, "state": "uploaded", "size": 42,
             "digest": f"sha256:{DIGEST}"},
            {"name": f"{ASSET}.sha256", "state": "uploaded", "size": 86,
             "url": "checksum-url"},
        ],
    }


class ReleaseLookupTests(unittest.TestCase):
    def test_published_tag_bypasses_duplicate_draft_listing(self):
        with patch.object(build_metal.subprocess, "run", return_value=subprocess.CompletedProcess(
            [], 0, json.dumps(release(False)), ""
        )) as lookup, patch.object(build_metal, "run", return_value=subprocess.CompletedProcess(
            [], 0, f"{DIGEST}  {ASSET}\n", ""
        )) as checksum:
            self.assertEqual(
                build_metal.existing_release("owner/repo", "metal-16A242d", ASSET), DIGEST
            )
            lookup.assert_called_once()
            checksum.assert_called_once()

    def test_listing_prefers_published_release_over_draft(self):
        with patch.object(build_metal.subprocess, "run", return_value=subprocess.CompletedProcess(
            [], 1, "", "HTTP 404"
        )), patch.object(build_metal, "run", side_effect=[
            subprocess.CompletedProcess([], 0, json.dumps([[release(True), release(False)]]), ""),
            subprocess.CompletedProcess([], 0, f"{DIGEST}  {ASSET}\n", ""),
        ]):
            self.assertEqual(
                build_metal.existing_release("owner/repo", "metal-16A242d", ASSET), DIGEST
            )

    def test_draft_alone_is_not_a_missing_release(self):
        with patch.object(build_metal.subprocess, "run", return_value=subprocess.CompletedProcess(
            [], 1, "", "HTTP 404"
        )), patch.object(build_metal, "run", return_value=subprocess.CompletedProcess(
            [], 0, json.dumps([[release(True)]]), ""
        )):
            with self.assertRaisesRegex(ValueError, "incomplete"):
                build_metal.existing_release("owner/repo", "metal-16A242d", ASSET)

    def test_lookup_errors_do_not_trigger_a_new_release(self):
        with patch.object(build_metal.subprocess, "run", return_value=subprocess.CompletedProcess(
            [], 1, "", "HTTP 403"
        )), patch.object(build_metal, "run") as listing:
            with self.assertRaisesRegex(ValueError, "HTTP 403"):
                build_metal.existing_release("owner/repo", "metal-16A242d", ASSET)
            listing.assert_not_called()

    def test_existing_tag_is_not_reuploaded_when_release_listing_misses_it(self):
        with patch.object(build_metal.subprocess, "run", side_effect=[
            subprocess.CompletedProcess([], 1, "", "HTTP 404"),
            subprocess.CompletedProcess([], 0, '{"ref":"refs/tags/metal-16A242d"}', ""),
        ]), patch.object(build_metal, "run", return_value=subprocess.CompletedProcess(
            [], 0, "[[]]", ""
        )):
            with self.assertRaisesRegex(ValueError, "exists but its release was not found"):
                build_metal.existing_release("owner/repo", "metal-16A242d", ASSET)

    def test_missing_tag_can_be_created(self):
        with patch.object(build_metal.subprocess, "run", return_value=subprocess.CompletedProcess(
            [], 1, "", "HTTP 404"
        )), patch.object(build_metal, "run", return_value=subprocess.CompletedProcess(
            [], 0, "[[]]", ""
        )):
            self.assertIsNone(
                build_metal.existing_release("owner/repo", "metal-16A242d", ASSET)
            )


if __name__ == "__main__":
    unittest.main()

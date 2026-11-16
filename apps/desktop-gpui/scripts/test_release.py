"""Tests for the macOS desktop release helpers."""

import hashlib
import tempfile
import unittest
from pathlib import Path

from release import (
    BUNDLE_ID,
    ReleaseError,
    archive_filename,
    cargo_build_environment,
    checksum_manifest,
    read_signing_statuses,
    release_note_from_statuses,
    signing_mode,
    validate_plist,
    validate_release_versions,
    validate_tag_version,
    write_signing_status,
)


class ReleaseTests(unittest.TestCase):
    def test_release_tag_must_exactly_match_the_package_version(self) -> None:
        self.assertEqual(validate_tag_version("v1.2.3", "1.2.3"), "1.2.3")
        with self.assertRaises(ReleaseError):
            validate_tag_version("v1.2.4", "1.2.3")
        with self.assertRaises(ReleaseError):
            validate_tag_version("1.2.3", "1.2.3")

    def test_release_tag_requires_root_and_app_package_versions_to_match(self) -> None:
        self.assertEqual(
            validate_release_versions("v1.2.3", "1.2.3", "1.2.3"),
            "1.2.3",
        )
        with self.assertRaises(ReleaseError):
            validate_release_versions("v1.2.3", "1.2.3", "1.2.2")
        with self.assertRaises(ReleaseError):
            validate_release_versions("v1.2.4", "1.2.3", "1.2.3")

    def test_plist_must_match_the_release_version_and_bundle_identity(self) -> None:
        plist = {
            "CFBundleIdentifier": BUNDLE_ID,
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": "1.2.3",
            "CFBundleVersion": "1.2.3",
        }

        validate_plist(plist, "1.2.3")

        with self.assertRaises(ReleaseError):
            validate_plist({**plist, "CFBundleVersion": "1.2.2"}, "1.2.3")
        with self.assertRaises(ReleaseError):
            validate_plist({**plist, "CFBundleIdentifier": "wrong.bundle"}, "1.2.3")

    def test_archive_names_encode_the_target_and_version_contract(self) -> None:
        self.assertEqual(
            archive_filename("aarch64-apple-darwin", "1.2.3"),
            "localsend-gpui-aarch64-apple-darwin-v1.2.3.tar.gz",
        )
        with self.assertRaises(ReleaseError):
            archive_filename("unknown-target", "1.2.3")

    def test_signing_is_unsigned_only_when_no_signing_secret_is_configured(self) -> None:
        self.assertEqual(signing_mode({}), "unsigned")
        self.assertEqual(
            signing_mode(
                {
                    "APPLE_DEVELOPER_ID_CERTIFICATE_P12_BASE64": "certificate",
                    "APPLE_DEVELOPER_ID_CERTIFICATE_PASSWORD": "password",
                    "APPLE_DEVELOPER_ID_SIGNING_IDENTITY": "Developer ID Application",
                    "APPLE_NOTARY_API_KEY_P8_BASE64": "key",
                    "APPLE_NOTARY_KEY_ID": "key-id",
                    "APPLE_NOTARY_ISSUER_ID": "issuer-id",
                    "APPLE_DEVELOPER_TEAM_ID": "ABCDEFGHIJ",
                }
            ),
            "signed",
        )
        self.assertEqual(
            signing_mode({"APPLE_DEVELOPER_TEAM_ID": "ABCDEFGHIJ"}),
            "unsigned",
        )
        with self.assertRaises(ReleaseError):
            signing_mode({"APPLE_DEVELOPER_ID_CERTIFICATE_P12_BASE64": "certificate"})
        with self.assertRaises(ReleaseError):
            signing_mode(
                {
                    "APPLE_DEVELOPER_ID_CERTIFICATE_P12_BASE64": "certificate",
                    "APPLE_DEVELOPER_ID_CERTIFICATE_PASSWORD": "password",
                    "APPLE_DEVELOPER_ID_SIGNING_IDENTITY": "Developer ID Application",
                    "APPLE_NOTARY_API_KEY_P8_BASE64": "key",
                    "APPLE_NOTARY_KEY_ID": "key-id",
                    "APPLE_NOTARY_ISSUER_ID": "issuer-id",
                }
            )

    def test_only_signed_cargo_builds_embed_the_pinned_team_id(self) -> None:
        base_environment = {
            "PATH": "path",
            "LOCALSEND_GPUI_TRUSTED_TEAM_ID": "UNTRUSTED00",
        }
        unsigned = cargo_build_environment(
            "unsigned", "ABCDEFGHIJ", base_environment
        )
        signed = cargo_build_environment("signed", "ABCDEFGHIJ", base_environment)

        self.assertNotIn("LOCALSEND_GPUI_TRUSTED_TEAM_ID", unsigned)
        self.assertEqual(signed["LOCALSEND_GPUI_TRUSTED_TEAM_ID"], "ABCDEFGHIJ")
        self.assertEqual(unsigned["PATH"], "path")
        with self.assertRaises(ReleaseError):
            cargo_build_environment("signed", "not-a-team-id", base_environment)

    def test_release_notes_label_signing_state_consistently_with_both_artifacts(self) -> None:
        targets = ["aarch64-apple-darwin", "x86_64-apple-darwin"]
        with tempfile.TemporaryDirectory() as temporary_directory:
            status_directory = Path(temporary_directory)
            for target in targets:
                write_signing_status(
                    status_directory,
                    target,
                    "unsigned",
                    "ABCDEFGHIJ",
                )
            statuses = read_signing_statuses(status_directory)

        unsigned = release_note_from_statuses(statuses)
        signed = release_note_from_statuses(
            {target: "signed:ABCDEFGHIJ" for target in targets}
        )

        self.assertIn("UNSIGNED AND NOT NOTARIZED", unsigned)
        self.assertIn("do not establish publisher identity", unsigned)
        self.assertIn("Developer ID signed and notarized", signed)
        self.assertIn("ABCDEFGHIJ", signed)
        with self.assertRaises(ReleaseError):
            release_note_from_statuses(
                {
                    "aarch64-apple-darwin": "unsigned",
                    "x86_64-apple-darwin": "signed:ABCDEFGHIJ",
                }
            )

    def test_checksum_manifest_hashes_sorted_archive_names(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            directory = Path(temporary_directory)
            first = directory / "localsend-gpui-z.tar.gz"
            second = directory / "localsend-gpui-a.tar.gz"
            first.write_bytes(b"z")
            second.write_bytes(b"a")

            manifest = checksum_manifest([first, second])

        self.assertEqual(
            manifest,
            f"{hashlib.sha256(b'a').hexdigest()}  localsend-gpui-a.tar.gz\n"
            f"{hashlib.sha256(b'z').hexdigest()}  localsend-gpui-z.tar.gz\n",
        )


if __name__ == "__main__":
    unittest.main()

"""Structural checks for the release workflow's repository-dependent steps."""

import re
import unittest
from pathlib import Path


class ReleaseWorkflowTests(unittest.TestCase):
    def test_signing_preflight_has_explicit_checkpoints_and_cannot_release(self) -> None:
        repository = Path(__file__).resolve().parents[3]
        workflow = (repository / ".github/workflows/signing-preflight.yml").read_text(
            encoding="utf-8"
        )
        self.assertRegex(workflow, r"(?m)^  workflow_dispatch:\s*$")
        self.assertRegex(workflow, r"(?m)^  contents: read\s*$")
        self.assertNotIn("gh release", workflow)
        self.assertIn("macos-15", workflow)
        self.assertIn("macos-15-intel", workflow)
        self.assertIn("security find-identity -v -p codesigning", workflow)
        self.assertIn('security list-keychains -d user -s "$KEYCHAIN_PATH"', workflow)
        self.assertIn("codesign --verify --strict", workflow)
        self.assertIn("xcrun notarytool history", workflow)
        self.assertIn("security delete-keychain", workflow)

    def test_manual_signing_check_cannot_publish_or_fall_back_to_unsigned(self) -> None:
        repository = Path(__file__).resolve().parents[3]
        workflow = (repository / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        self.assertRegex(workflow, r"(?m)^  workflow_dispatch:\s*$")
        macos = workflow.split("\n  macos-app:\n", 1)[1].split("\n  release:\n", 1)[0]
        self.assertIn("github.event_name == 'workflow_dispatch'", macos)
        self.assertIn("steps.signing.outputs.mode != 'signed'", macos)
        self.assertRegex(macos, r"(?m)^\s+run: exit 1\s*$")
        self.assertIn("spctl --assess --type execute", macos)
        release = workflow.split("\n  release:\n", 1)[1]
        self.assertRegex(release, r"(?m)^    if: github\.event_name == 'push'\s*$")
        self.assertIn("gh release create", release)
        self.assertIn("needs.version.outputs.tag", macos)

    def test_release_signing_exposes_the_temporary_keychain_to_codesign(self) -> None:
        repository = Path(__file__).resolve().parents[3]
        workflow = (repository / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        macos = workflow.split("\n  macos-app:\n", 1)[1].split("\n  release:\n", 1)[0]
        search_list = macos.find('security list-keychains -d user -s "$KEYCHAIN_PATH"')
        codesign = macos.find("codesign --force --deep")
        self.assertGreaterEqual(search_list, 0, "temporary keychain must be searchable")
        self.assertGreater(codesign, search_list, "configure search list before signing")

    def test_signed_artifact_is_checked_by_the_real_updater_verifier(self) -> None:
        repository = Path(__file__).resolve().parents[3]
        workflow = (repository / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        macos = workflow.split("\n  macos-app:\n", 1)[1].split("\n  release:\n", 1)[0]
        check = macos.find(
            "update::tests::real_signed_bundle_accepts_the_pinned_team_and_rejects_a_different_team"
        )
        staple = macos.find('xcrun stapler validate "$APP"')
        archive = macos.find("python3 scripts/release.py archive")
        self.assertGreater(check, staple, "verify the notarized app with the updater")
        self.assertGreater(archive, check, "verify before archiving")

    def test_manual_check_uses_the_package_version_not_the_branch_name(self) -> None:
        repository = Path(__file__).resolve().parents[3]
        workflow = (repository / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        version = workflow.split("\n  version:\n", 1)[1].split("\n  build:\n", 1)[0]
        self.assertIn("github.event_name", version)
        self.assertIn("package_metadata()", version)
        self.assertIn('scripts/release.py check-version --tag "$TAG"', version)
        self.assertIn('printf', version)
        self.assertIn('GITHUB_OUTPUT', version)

    def test_release_job_checks_out_the_repository_before_running_its_script(self) -> None:
        repository = Path(__file__).resolve().parents[3]
        workflow = (repository / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        release_job_start = workflow.find("\n  release:\n")
        self.assertGreaterEqual(release_job_start, 0, "release job must exist")
        release_job_body = release_job_start + len("\n  release:\n")
        next_job = re.search(
            r"(?m)^  [A-Za-z0-9_-]+:\s*$",
            workflow[release_job_body:],
        )
        release_job_end = (
            release_job_body + next_job.start()
            if next_job is not None
            else len(workflow)
        )
        release_job = workflow[release_job_start:release_job_end]

        checkout = release_job.find("uses: actions/checkout@")
        script_invocation = release_job.find(
            "python3 apps/desktop-gpui/scripts/release.py"
        )
        self.assertGreaterEqual(script_invocation, 0, "release job must run release.py")
        self.assertGreaterEqual(
            checkout,
            0,
            "release job must check out the repository before running its script",
        )
        self.assertLess(
            checkout,
            script_invocation,
            "release job checkout must precede release.py",
        )


if __name__ == "__main__":
    unittest.main()

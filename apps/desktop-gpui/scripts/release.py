#!/usr/bin/env python3
"""Build, validate, and package the macOS desktop app for GitHub releases."""

import argparse
import hashlib
import json
import os
import platform
import plistlib
import subprocess
import sys
from pathlib import Path
from typing import Dict, Mapping, Optional, Sequence, Tuple


APP_DIR = Path(__file__).resolve().parents[1]
REPOSITORY_DIR = APP_DIR.parents[1]
MANIFEST_PATH = APP_DIR / "Cargo.toml"
LIBRARY_MANIFEST_PATH = REPOSITORY_DIR / "Cargo.toml"
BUNDLE_NAME = "LocalSend RS.app"
BUNDLE_ID = "app.crosscopy.localsend-gpui"
EXECUTABLE_NAME = "localsend-gpui"
TRUSTED_TEAM_ID_ENV = "LOCALSEND_GPUI_TRUSTED_TEAM_ID"
TEAM_ID_INPUT_NAME = "APPLE_DEVELOPER_TEAM_ID"
TARGET_ARCHITECTURES = {
    "aarch64-apple-darwin": "arm64",
    "x86_64-apple-darwin": "x86_64",
}
RELEASE_TARGETS = tuple(TARGET_ARCHITECTURES)
SIGNING_SECRET_NAMES = (
    "APPLE_DEVELOPER_ID_CERTIFICATE_P12_BASE64",
    "APPLE_DEVELOPER_ID_CERTIFICATE_PASSWORD",
    "APPLE_DEVELOPER_ID_SIGNING_IDENTITY",
    "APPLE_NOTARY_API_KEY_P8_BASE64",
    "APPLE_NOTARY_KEY_ID",
    "APPLE_NOTARY_ISSUER_ID",
)


class ReleaseError(Exception):
    """A release input or artifact does not satisfy the release contract."""


def validate_tag_version(tag: str, package_version: str) -> str:
    """Require a v-prefixed tag to exactly match Cargo's package version."""
    if not tag.startswith("v") or tag[1:] != package_version:
        raise ReleaseError(
            "release tag must be exactly "
            f"v{package_version}; received {tag!r}"
        )
    return package_version


def validate_release_versions(
    tag: str, app_version: str, library_version: str
) -> str:
    """Require the tag, desktop package, and root library versions to agree."""
    if app_version != library_version:
        raise ReleaseError(
            "localsend-gpui and localsend-rs Cargo package versions must match; "
            f"received {app_version!r} and {library_version!r}"
        )
    return validate_tag_version(tag, app_version)


def validate_plist(plist: Mapping[str, object], version: str) -> None:
    """Check the bundle identity and both version fields against Cargo."""
    expected = {
        "CFBundleIdentifier": BUNDLE_ID,
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": version,
        "CFBundleVersion": version,
    }
    for key, expected_value in expected.items():
        if plist.get(key) != expected_value:
            raise ReleaseError(
                f"Info.plist {key} must be {expected_value!r}; "
                f"received {plist.get(key)!r}"
            )


def archive_filename(target: str, version: str) -> str:
    """Return the stable archive filename used by consumers and release notes."""
    if target not in TARGET_ARCHITECTURES:
        raise ReleaseError(f"unsupported macOS release target: {target}")
    return f"localsend-gpui-{target}-v{version}.tar.gz"


def signing_mode(values: Mapping[str, str]) -> str:
    """Select signing only with complete credentials and a pinned Team ID."""
    configured = [bool(values.get(name, "").strip()) for name in SIGNING_SECRET_NAMES]
    if not any(configured):
        return "unsigned"
    if not all(configured):
        missing = [
            name for name in SIGNING_SECRET_NAMES if not values.get(name, "").strip()
        ]
        raise ReleaseError(
            "Apple signing configuration is incomplete; missing: "
            + ", ".join(missing)
        )
    trusted_team_id = values.get(TEAM_ID_INPUT_NAME, "").strip()
    if not trusted_team_id:
        raise ReleaseError(
            f"{TEAM_ID_INPUT_NAME} must be configured before signing releases"
        )
    validate_team_id(trusted_team_id)
    return "signed"


def validate_team_id(team_id: str) -> str:
    """Validate the immutable Apple Team ID embedded in signed updater builds."""
    if len(team_id) != 10 or not all(
        character.isascii() and (character.isupper() or character.isdigit())
        for character in team_id
    ):
        raise ReleaseError("the trusted Apple Team ID must be 10 uppercase letters or digits")
    return team_id


def cargo_build_environment(
    mode: str,
    trusted_team_id: str,
    base_environment: Optional[Mapping[str, str]] = None,
) -> Dict[str, str]:
    """Pass the trusted Team ID to Cargo only for an explicitly signed build."""
    environment = dict(os.environ if base_environment is None else base_environment)
    environment.pop(TRUSTED_TEAM_ID_ENV, None)
    if mode == "unsigned":
        return environment
    if mode != "signed":
        raise ReleaseError(f"unsupported signing mode: {mode}")
    environment[TRUSTED_TEAM_ID_ENV] = validate_team_id(trusted_team_id)
    return environment


def checksum_manifest(paths: Sequence[Path]) -> str:
    """Format SHA-256 lines for release archives in filename order."""
    lines = []
    for path in sorted(paths, key=lambda item: item.name):
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        lines.append(f"{digest}  {path.name}\n")
    return "".join(lines)


def cargo_metadata(manifest_path: Path) -> Mapping[str, object]:
    """Read Cargo metadata for a single manifest without resolving dependencies."""
    result = subprocess.run(
        [
            "cargo",
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
            str(manifest_path),
        ],
        cwd=manifest_path.parent,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def package_version(manifest_path: Path, name: str) -> str:
    metadata = cargo_metadata(manifest_path)
    packages = [
        package
        for package in metadata["packages"]
        if package["name"] == name
    ]
    if len(packages) != 1:
        raise ReleaseError(f"Cargo metadata must contain exactly one {name} package")
    return packages[0]["version"]


def package_metadata() -> Tuple[str, Path]:
    """Read the app package version and Cargo's target output directory."""
    metadata = cargo_metadata(MANIFEST_PATH)
    version = package_version_from_metadata(metadata, "localsend-gpui")
    return version, Path(metadata["target_directory"])


def package_version_from_metadata(metadata: Mapping[str, object], name: str) -> str:
    packages = [
        package
        for package in metadata["packages"]
        if package["name"] == name
    ]
    if len(packages) != 1:
        raise ReleaseError(f"Cargo metadata must contain exactly one {name} package")
    return packages[0]["version"]


def release_version(tag: str) -> str:
    app_version, _ = package_metadata()
    library_version = package_version(LIBRARY_MANIFEST_PATH, "localsend-rs")
    return validate_release_versions(tag, app_version, library_version)


def release_note_from_statuses(statuses: Mapping[str, str]) -> str:
    """Render a downloader-visible trust label after both app jobs finish."""
    if set(statuses) != set(RELEASE_TARGETS):
        raise ReleaseError("release status must include both macOS architectures")
    values = [statuses[target].strip() for target in RELEASE_TARGETS]
    if values[0] != values[1]:
        raise ReleaseError("macOS release archives have inconsistent signing status")
    status = values[0]
    if status == "unsigned":
        return (
            "### macOS app signing\n\n"
            "**UNSIGNED AND NOT NOTARIZED.** The macOS app archives do not "
            "establish publisher identity, and checksum verification alone "
            "does not authenticate the publisher."
        )
    if status.startswith("signed:"):
        team_id = validate_team_id(status.removeprefix("signed:"))
        return (
            "### macOS app signing\n\n"
            f"Developer ID signed and notarized for Apple Team ID `{team_id}`. "
            "The desktop updater trusts this Team ID only when it was pinned "
            "into the app at build time."
        )
    raise ReleaseError(f"unrecognized macOS signing status: {status!r}")


def signing_status_filename(target: str) -> str:
    if target not in TARGET_ARCHITECTURES:
        raise ReleaseError(f"unsupported macOS release target: {target}")
    return f"MACOS-SIGNING-{target}.txt"


def write_signing_status(
    directory: Path, target: str, mode: str, trusted_team_id: str
) -> Path:
    if mode == "signed":
        status = f"signed:{validate_team_id(trusted_team_id)}"
    elif mode == "unsigned":
        status = "unsigned"
    else:
        raise ReleaseError(f"unsupported signing mode: {mode}")
    directory.mkdir(parents=True, exist_ok=True)
    status_path = directory / signing_status_filename(target)
    status_path.write_text(f"{status}\n", encoding="utf-8")
    return status_path


def read_signing_statuses(directory: Path) -> Mapping[str, str]:
    statuses: Dict[str, str] = {}
    for target in RELEASE_TARGETS:
        status_path = directory / signing_status_filename(target)
        try:
            statuses[target] = status_path.read_text(encoding="utf-8").strip()
        except OSError as error:
            raise ReleaseError(f"cannot read {status_path}: {error}") from error
    return statuses


def require_native_target(target: str) -> str:
    """Refuse to build or run an app for an architecture other than this runner."""
    if platform.system() != "Darwin":
        raise ReleaseError("macOS app packaging must run on a macOS host")
    expected = TARGET_ARCHITECTURES.get(target)
    if expected is None:
        raise ReleaseError(f"unsupported macOS release target: {target}")
    machine = platform.machine().lower()
    host_arch = {
        "aarch64": "arm64",
        "arm64": "arm64",
        "x86_64": "x86_64",
        "amd64": "x86_64",
    }.get(machine)
    if host_arch != expected:
        raise ReleaseError(
            f"refusing to run a {expected} package on {machine or 'unknown'} host"
        )
    return expected


def require_binary_architecture(binary: Path, expected: str) -> None:
    """Fail closed unless the Mach-O executable has exactly the requested arch."""
    result = subprocess.run(
        ["lipo", "-archs", str(binary)],
        check=True,
        capture_output=True,
        text=True,
    )
    architectures = set(result.stdout.split())
    if architectures != {expected}:
        raise ReleaseError(
            f"{binary} architectures must be exactly {expected}; "
            f"received {', '.join(sorted(architectures)) or 'none'}"
        )


def read_and_validate_plist(app_path: Path, version: str) -> None:
    plist_path = app_path / "Contents" / "Info.plist"
    try:
        with plist_path.open("rb") as plist_file:
            plist = plistlib.load(plist_file)
    except (OSError, plistlib.InvalidFileException) as error:
        raise ReleaseError(f"cannot read {plist_path}: {error}") from error
    validate_plist(plist, version)


def build_app(
    tag: str,
    target: str,
    output_dir: Path,
    mode: str,
    trusted_team_id: str,
) -> None:
    """Build for the matching native runner, then use the app's bundle command."""
    expected_arch = require_native_target(target)
    version = release_version(tag)
    _, target_dir = package_metadata()
    binary = target_dir / target / "release" / EXECUTABLE_NAME

    subprocess.run(
        [
            "cargo",
            "build",
            "--release",
            "--package",
            "localsend-gpui",
            "--bin",
            EXECUTABLE_NAME,
            "--target",
            target,
            "--manifest-path",
            str(MANIFEST_PATH),
        ],
        cwd=APP_DIR,
        check=True,
        env=cargo_build_environment(mode, trusted_team_id),
    )
    require_binary_architecture(binary, expected_arch)

    output_dir.mkdir(parents=True, exist_ok=True)
    # The executable is invoked only after checking both its Mach-O architecture
    # and the host architecture. "bundle" copies this same executable into the app.
    subprocess.run(
        [str(binary), "bundle", "--dir", str(output_dir)],
        cwd=APP_DIR,
        check=True,
    )
    app_path = output_dir / BUNDLE_NAME
    read_and_validate_plist(app_path, version)
    require_binary_architecture(
        app_path / "Contents" / "MacOS" / EXECUTABLE_NAME,
        expected_arch,
    )


def create_archive(
    tag: str, target: str, app_path: Path, output_dir: Path
) -> Path:
    """Validate a completed .app and create its versioned tar.gz archive."""
    version = release_version(tag)
    expected_arch = TARGET_ARCHITECTURES.get(target)
    if expected_arch is None:
        raise ReleaseError(f"unsupported macOS release target: {target}")
    read_and_validate_plist(app_path, version)
    require_binary_architecture(
        app_path / "Contents" / "MacOS" / EXECUTABLE_NAME,
        expected_arch,
    )

    output_dir.mkdir(parents=True, exist_ok=True)
    archive_path = output_dir / archive_filename(target, version)
    subprocess.run(
        [
            "tar",
            "-czf",
            str(archive_path),
            "-C",
            str(app_path.parent),
            app_path.name,
        ],
        check=True,
    )
    return archive_path


def check_version(args: argparse.Namespace) -> None:
    version = release_version(args.tag)
    print(f"Release tag {args.tag} matches localsend-gpui and localsend-rs {version}.")


def choose_signing_mode(args: argparse.Namespace) -> None:
    mode = signing_mode(os.environ)
    if args.github_output:
        with Path(args.github_output).open("a", encoding="utf-8") as output:
            output.write(f"mode={mode}\n")
    print(mode)


def package_command(args: argparse.Namespace) -> None:
    build_app(
        args.tag,
        args.target,
        Path(args.output_dir).resolve(),
        args.signing_mode,
        args.team_id,
    )


def archive_command(args: argparse.Namespace) -> None:
    archive_path = create_archive(
        args.tag,
        args.target,
        Path(args.app_path).resolve(),
        Path(args.output_dir).resolve(),
    )
    print(archive_path)


def checksums_command(args: argparse.Namespace) -> None:
    directory = Path(args.directory)
    archives = sorted(
        path
        for path in directory.iterdir()
        if path.is_file() and (path.name.endswith(".tar.gz") or path.suffix == ".zip")
    )
    if not archives:
        raise ReleaseError(f"no release archives found in {directory}")
    manifest_path = directory / "SHA256SUMS"
    manifest_path.write_text(checksum_manifest(archives), encoding="utf-8")
    print(manifest_path)


def signing_status_command(args: argparse.Namespace) -> None:
    path = write_signing_status(
        Path(args.directory), args.target, args.mode, args.team_id
    )
    print(path)


def release_note_command(args: argparse.Namespace) -> None:
    directory = Path(args.directory)
    note = release_note_from_statuses(read_signing_statuses(directory))
    (directory / "MACOS-ARTIFACT-STATUS.txt").write_text(
        f"{note}\n", encoding="utf-8"
    )
    print(note)


def parser() -> argparse.ArgumentParser:
    command_parser = argparse.ArgumentParser(description=__doc__)
    commands = command_parser.add_subparsers(dest="command", required=True)

    version_parser = commands.add_parser("check-version")
    version_parser.add_argument("--tag", required=True)
    version_parser.set_defaults(run=check_version)

    signing_parser = commands.add_parser("signing-mode")
    signing_parser.add_argument("--github-output")
    signing_parser.set_defaults(run=choose_signing_mode)

    package_parser = commands.add_parser("package")
    package_parser.add_argument("--tag", required=True)
    package_parser.add_argument("--target", required=True)
    package_parser.add_argument("--output-dir", required=True)
    package_parser.add_argument(
        "--signing-mode", choices=("signed", "unsigned"), required=True
    )
    package_parser.add_argument("--team-id", default="")
    package_parser.set_defaults(run=package_command)

    archive_parser = commands.add_parser("archive")
    archive_parser.add_argument("--tag", required=True)
    archive_parser.add_argument("--target", required=True)
    archive_parser.add_argument("--app-path", required=True)
    archive_parser.add_argument("--output-dir", required=True)
    archive_parser.set_defaults(run=archive_command)

    checksums_parser = commands.add_parser("checksums")
    checksums_parser.add_argument("--directory", required=True)
    checksums_parser.set_defaults(run=checksums_command)

    status_parser = commands.add_parser("signing-status")
    status_parser.add_argument("--directory", required=True)
    status_parser.add_argument("--target", required=True)
    status_parser.add_argument("--mode", choices=("signed", "unsigned"), required=True)
    status_parser.add_argument("--team-id", default="")
    status_parser.set_defaults(run=signing_status_command)

    note_parser = commands.add_parser("release-note")
    note_parser.add_argument("--directory", required=True)
    note_parser.set_defaults(run=release_note_command)
    return command_parser


def main() -> int:
    args = parser().parse_args()
    try:
        args.run(args)
    except (ReleaseError, OSError, subprocess.CalledProcessError) as error:
        print(f"release: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Render the Homebrew cask and formula from the published release archives.

The checked-in files under ``packaging/homebrew`` are the source of truth. A
release re-renders them so the version and every sha256 come from the archives
that were actually uploaded, not from a hand-typed value.

Usage (from ``apps/desktop-gpui``, or CI):

    python3 scripts/homebrew.py --version 0.2.1 --assets-dir /tmp/release
"""

import argparse
import hashlib
import re
import sys
from pathlib import Path
from typing import Dict, List, Optional, Tuple

APP_DIR = Path(__file__).resolve().parents[1]
REPOSITORY_DIR = APP_DIR.parents[1]
DEFAULT_PACKAGING_DIR = REPOSITORY_DIR / "packaging" / "homebrew"

CASK_PATH = Path("Casks") / "localsend-rs.rb"
FORMULA_PATH = Path("Formula") / "localsend-rs-cli.rb"

# Archives each file installs, in the order its sha256 values appear.
CASK_ARCHIVES = (
    "localsend-gpui-aarch64-apple-darwin-v{version}.tar.gz",
    "localsend-gpui-x86_64-apple-darwin-v{version}.tar.gz",
)
FORMULA_ARCHIVES = (
    "localsend-rs-aarch64-apple-darwin-v{version}.tar.gz",
    "localsend-rs-x86_64-apple-darwin-v{version}.tar.gz",
    "localsend-rs-aarch64-unknown-linux-musl-v{version}.tar.gz",
    "localsend-rs-x86_64-unknown-linux-musl-v{version}.tar.gz",
)


class RenderError(Exception):
    """The archives or a checked-in file do not match the release contract."""


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def archive_checksums(
    assets_dir: Path, version: str, patterns: Tuple[str, ...]
) -> List[str]:
    """Return each archive's sha256, refusing to render a partial release."""
    checksums = []
    missing = []
    for pattern in patterns:
        path = assets_dir / pattern.format(version=version)
        if not path.is_file():
            missing.append(path.name)
            continue
        checksums.append(sha256(path))
    if missing:
        raise RenderError("missing release archives: " + ", ".join(missing))
    return checksums


def _replace_version(text: str, version: str) -> str:
    needle = 'version "'
    start = text.find(needle)
    if start < 0:
        raise RenderError('file has no version "..." line')
    end = text.find('"', start + len(needle))
    if end < 0:
        raise RenderError("version line is not terminated")
    return text[: start + len(needle)] + version + text[end:]


def _current_version(text: str) -> str:
    needle = 'version "'
    start = text.find(needle)
    if start < 0:
        raise RenderError('the cask has no version "..." line')
    end = text.find('"', start + len(needle))
    if end < 0:
        raise RenderError("the cask's version line is not terminated")
    return text[start + len(needle) : end]


def _replace_release_version(text: str, previous: str, version: str) -> str:
    """Rewrite the concrete `v<version>` segments of the formula's URLs.

    The formula has no `version` stanza (Homebrew 7 rejects one that repeats
    the URL's version), so the URLs are what the release renderer updates.
    """
    pattern = re.compile(r"(?<![\w.])v" + re.escape(previous) + r"(?!\d)")
    if not pattern.search(text):
        raise RenderError(
            f"the formula does not name v{previous}; refusing to guess its version"
        )
    return pattern.sub("v" + version, text)


def _replace_sha256(text: str, checksums: List[str], name: str) -> str:
    marker = '"'
    values = []
    index = 0
    while True:
        index = text.find(marker, index)
        if index < 0:
            break
        end = text.find(marker, index + 1)
        if end < 0:
            raise RenderError(f"{name} has an unterminated string")
        value = text[index + 1 : end]
        if len(value) == 64 and all(character in "0123456789abcdef" for character in value):
            values.append((index + 1, end))
        index = end + 1

    if len(values) != len(checksums):
        raise RenderError(
            f"{name} must contain {len(checksums)} sha256 values; found {len(values)}"
        )

    rendered = text
    for (start, end), digest in zip(reversed(values), reversed(checksums)):
        rendered = rendered[:start] + digest + rendered[end:]
    return rendered


def render_cask(path: Path, version: str, checksums: List[str]) -> str:
    text = path.read_text(encoding="utf-8")
    text = _replace_version(text, version)
    return _replace_sha256(text, checksums, path.name)


def render_formula(
    path: Path, previous: str, version: str, checksums: List[str]
) -> str:
    text = path.read_text(encoding="utf-8")
    text = _replace_release_version(text, previous, version)
    return _replace_sha256(text, checksums, path.name)


def render_packaging(
    packaging_dir: Path, assets_dir: Path, version: str
) -> Dict[Path, Path]:
    """Render both tap files in place and return their written paths."""
    if not version or version.startswith("v"):
        raise RenderError("version must be the bare release version, for example 0.2.1")

    cask_path = packaging_dir / CASK_PATH
    formula_path = packaging_dir / FORMULA_PATH
    for path in (cask_path, formula_path):
        if not path.is_file():
            raise RenderError(f"missing checked-in tap file: {path}")

    cask_source = cask_path.read_text(encoding="utf-8")
    previous = _current_version(cask_source)
    cask = render_cask(
        cask_path, version, archive_checksums(assets_dir, version, CASK_ARCHIVES)
    )
    formula = render_formula(
        formula_path,
        previous,
        version,
        archive_checksums(assets_dir, version, FORMULA_ARCHIVES),
    )

    cask_path.write_text(cask, encoding="utf-8")
    formula_path.write_text(formula, encoding="utf-8")
    return {cask_path: cask_path, formula_path: formula_path}


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True, help="bare version, e.g. 0.2.1")
    parser.add_argument(
        "--assets-dir",
        required=True,
        type=Path,
        help="directory containing the published localsend-rs release archives",
    )
    parser.add_argument(
        "--packaging-dir",
        type=Path,
        default=DEFAULT_PACKAGING_DIR,
        help="packaging/homebrew checkout to update in place",
    )
    arguments = parser.parse_args(argv)

    try:
        written = render_packaging(
            arguments.packaging_dir, arguments.assets_dir, arguments.version
        )
    except RenderError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    for path in written:
        print(f"rendered {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

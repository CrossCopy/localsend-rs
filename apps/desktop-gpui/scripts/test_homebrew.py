"""The Homebrew renderer fills version and every sha256 from real archives."""

import hashlib
import importlib.util
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent


def load_homebrew():
    spec = importlib.util.spec_from_file_location("homebrew", HERE / "homebrew.py")
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


homebrew = load_homebrew()
REPOSITORY = HERE.parents[2]


class HomebrewRenderTests(unittest.TestCase):
    def make_release(self, directory: Path, version: str) -> dict[str, str]:
        checksums = {}
        for pattern in homebrew.CASK_ARCHIVES + homebrew.FORMULA_ARCHIVES:
            name = pattern.format(version=version)
            payload = name.encode("utf-8")
            (directory / name).write_bytes(payload)
            checksums[name] = hashlib.sha256(payload).hexdigest()
        return checksums

    def copy_templates(self, directory: Path) -> Path:
        packaging = directory / "packaging" / "homebrew"
        for relative in (homebrew.CASK_PATH, homebrew.FORMULA_PATH):
            destination = packaging / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes((REPOSITORY / "packaging" / "homebrew" / relative).read_bytes())
        return packaging

    def test_renders_the_published_version_and_checksums(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            assets = root / "assets"
            assets.mkdir()
            packaging = self.copy_templates(root)
            checksums = self.make_release(assets, "9.8.7")

            homebrew.render_packaging(packaging, assets, "9.8.7")

            cask = (packaging / homebrew.CASK_PATH).read_text(encoding="utf-8")
            formula = (packaging / homebrew.FORMULA_PATH).read_text(encoding="utf-8")
            self.assertIn('version "9.8.7"', cask)
            self.assertNotIn('version "9.8.7"', formula)
            self.assertIn(
                "releases/download/v9.8.7/localsend-rs-aarch64-apple-darwin-v9.8.7.tar.gz",
                formula,
            )
            self.assertNotIn("v0.2.1", formula)
            for name in homebrew.CASK_ARCHIVES:
                self.assertIn(checksums[name.format(version="9.8.7")], cask)
            for name in homebrew.FORMULA_ARCHIVES:
                self.assertIn(checksums[name.format(version="9.8.7")], formula)
            self.assertIn("localsend-gpui-#{arch}-apple-darwin-v#{version}", cask)

    def test_missing_archive_is_an_error_not_a_stale_checksum(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            assets = root / "assets"
            assets.mkdir()
            packaging = self.copy_templates(root)
            self.make_release(assets, "1.2.3")
            missing = assets / "localsend-rs-x86_64-apple-darwin-v1.2.3.tar.gz"
            missing.unlink()
            with self.assertRaises(homebrew.RenderError):
                homebrew.render_packaging(packaging, assets, "1.2.3")

    def test_version_must_be_bare(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            assets = root / "assets"
            assets.mkdir()
            packaging = self.copy_templates(root)
            with self.assertRaises(homebrew.RenderError):
                homebrew.render_packaging(packaging, assets, "v1.2.3")


if __name__ == "__main__":
    unittest.main()

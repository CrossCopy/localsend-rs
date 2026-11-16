#!/usr/bin/env python3
"""Regression tests for the bronze mark's generated app and sidebar icons."""

from pathlib import Path
import base64
from io import BytesIO
import importlib.util
import unittest
from xml.etree import ElementTree

from PIL import Image

ASSETS = Path(__file__).parent
SPEC = importlib.util.spec_from_file_location("make_icon", ASSETS / "make-icon.py")
assert SPEC is not None and SPEC.loader is not None
make_icon = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(make_icon)

SIZE = 1024
MIN_INSET = 128
MAX_INSET = 160


class IconGridTests(unittest.TestCase):
    def test_source_visible_bounds_ignore_transparent_fringe(self) -> None:
        with Image.open(make_icon.SOURCE_MARK) as source:
            alpha = source.convert("RGBA").getchannel("A")
            visible = alpha.point(lambda value: 255 if value >= 16 else 0)
        self.assertEqual(visible.getbbox(), (179, 182, 1072, 1054))

    def test_clean_source_removes_faint_pixels_without_recoloring(self) -> None:
        with Image.open(make_icon.SOURCE_MARK) as original:
            original = original.convert("RGBA")
            cleaned = make_icon.load_source_mark()

        self.assertEqual(cleaned.size, (893, 872))
        self.assertEqual(cleaned.getpixel((625 - 179, 618 - 182)), original.getpixel((625, 618)))
        self.assertTrue(
            all(alpha == 0 or alpha >= 16 for alpha in cleaned.getchannel("A").getdata())
        )

    def test_mark_is_centered_with_platform_icon_padding(self) -> None:
        for filename in ("Mark-Light.png", "Mark-Dark.png"):
            with self.subTest(filename=filename):
                with Image.open(ASSETS / filename) as image:
                    mark = image.convert("RGBA")
                alpha = mark.getchannel("A")
                self.assertEqual(mark.size, (SIZE, SIZE))
                bounds = alpha.getbbox()
                self.assertIsNotNone(bounds)
                left, top, right, bottom = bounds
                for inset in (left, top, SIZE - right, SIZE - bottom):
                    self.assertGreaterEqual(inset, MIN_INSET)
                    self.assertLessEqual(inset, MAX_INSET)
                self.assertLessEqual(abs((left + right) / 2 - SIZE / 2), 2)
                self.assertLessEqual(abs((top + bottom) / 2 - SIZE / 2), 2)
                self.assertFalse(
                    any(0 < value < 16 for value in alpha.getdata()),
                    f"{filename} contains faint fringe pixels",
                )

    def test_theme_tiles_differ_without_recoloring_the_source_mark(self) -> None:
        with Image.open(ASSETS / "AppIcon-Dark.png") as image:
            dark = image.convert("RGBA")
        with Image.open(ASSETS / "AppIcon-Light.png") as image:
            light = image.convert("RGBA")
        mark = make_icon.render_mark()

        self.assertEqual(dark.size, (SIZE, SIZE))
        self.assertEqual(light.size, (SIZE, SIZE))
        self.assertNotEqual(dark.getpixel((512, 100)), light.getpixel((512, 100)))
        self.assertLessEqual(
            max(
                abs(
                    dark.getpixel((512, 512))[channel]
                    - light.getpixel((512, 512))[channel]
                )
                for channel in range(3)
            ),
            2,
        )
        with Image.open(make_icon.SOURCE_MARK) as source:
            source_color = source.convert("RGBA").getpixel((625, 618))
        mark_color = mark.getpixel((512, 512))
        self.assertLessEqual(
            max(abs(mark_color[channel] - source_color[channel]) for channel in range(3)),
            10,
        )
        self.assertEqual(mark_color[3], source_color[3])

    def test_svg_logo_is_a_png_wrapper_not_a_redrawn_mark(self) -> None:
        for suffix in ("Light", "Dark"):
            with self.subTest(suffix=suffix):
                root = ElementTree.parse(ASSETS / f"logo-{suffix.lower()}.svg").getroot()
                image = root.find("{http://www.w3.org/2000/svg}image")
                self.assertIsNotNone(image)
                self.assertEqual(root.attrib["width"], str(SIZE))
                self.assertEqual(root.attrib["height"], str(SIZE))
                self.assertTrue(image.attrib["href"].startswith("data:image/png;base64,"))
                embedded_png = base64.b64decode(image.attrib["href"].split(",", 1)[1])
                with Image.open(BytesIO(embedded_png)) as embedded:
                    with Image.open(ASSETS / f"AppIcon-{suffix}.png") as app_icon:
                        self.assertEqual(
                            embedded.convert("RGBA").tobytes(),
                            app_icon.convert("RGBA").tobytes(),
                        )

    def test_legacy_and_system_icon_outputs_are_compiled_formats(self) -> None:
        self.assertEqual((ASSETS / "AppIcon.icns").read_bytes()[:4], b"icns")
        self.assertIn(
            b"NSAppearanceNameDarkAqua", (ASSETS / "Assets.car").read_bytes()
        )


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Build every LocalSend GPUI icon from the supplied transparent bronze mark.

The PNG in this directory is the only artwork source. The light and dark
appearances share that unmodified mark and differ only in their tile treatment.
"""

import base64
import io
import json
import pathlib
import shutil
import subprocess
import tempfile
from typing import Optional

from PIL import Image, ImageDraw

SIZE = 1024
HERE = pathlib.Path(__file__).parent
SOURCE_MARK = HERE / "bronze-mark-source.png"
ALPHA_THRESHOLD = 16
MARK_VISIBLE_SIZE = 736
TILE_RADIUS = 232

THEMES = {
    "dark": {"tile": ("#12242F", "#0A141B")},
    "light": {"tile": ("#FFFFFF", "#E7EFF3")},
}


def load_source_mark() -> Image.Image:
    """Trim alpha speckle while preserving all source RGB and usable antialias."""
    with Image.open(SOURCE_MARK) as source:
        mark = source.convert("RGBA")

    alpha = mark.getchannel("A").point(
        lambda value: value if value >= ALPHA_THRESHOLD else 0
    )
    mark.putalpha(alpha)
    bounds = alpha.getbbox()
    if bounds is None:
        raise ValueError(f"{SOURCE_MARK} contains no visible mark")
    return mark.crop(bounds)


def render_mark() -> Image.Image:
    """Return the centered, square, transparent Icon Composer/sidebar layer."""
    source = load_source_mark()
    scale = MARK_VISIBLE_SIZE / max(source.size)
    width = round(source.width * scale)
    height = round(source.height * scale)
    resized = source.resize((width, height), Image.Resampling.LANCZOS)

    alpha = resized.getchannel("A").point(
        lambda value: value if value >= ALPHA_THRESHOLD else 0
    )
    resized.putalpha(alpha)
    bounds = alpha.getbbox()
    if bounds is None:
        raise ValueError("The mark vanished while being resized")
    resized = resized.crop(bounds)

    layer = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    x = (SIZE - resized.width) // 2
    y = (SIZE - resized.height) // 2
    layer.alpha_composite(resized, (x, y))
    return layer


def _rgb(hex_colour: str) -> tuple[int, int, int]:
    value = hex_colour.removeprefix("#")
    return tuple(int(value[index : index + 2], 16) for index in (0, 2, 4))


def _tile(theme: str) -> Image.Image:
    try:
        top, bottom = THEMES[theme]["tile"]
    except KeyError as error:
        raise ValueError(f"Unknown icon appearance: {theme}") from error

    top_rgb = _rgb(top)
    bottom_rgb = _rgb(bottom)
    tile = Image.new("RGBA", (SIZE, SIZE))
    draw = ImageDraw.Draw(tile)
    for y in range(SIZE):
        fraction = y / (SIZE - 1)
        colour = tuple(
            round(start + (end - start) * fraction)
            for start, end in zip(top_rgb, bottom_rgb)
        )
        draw.line((0, y, SIZE, y), fill=(*colour, 255))

    mask = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (0, 0, SIZE - 1, SIZE - 1), radius=TILE_RADIUS, fill=255
    )
    tile.putalpha(mask)
    return tile


def render_icon(theme: str, mark: Optional[Image.Image] = None) -> Image.Image:
    """Compose a standalone app icon with the source mark and theme tile."""
    icon = _tile(theme)
    icon.alpha_composite(mark if mark is not None else render_mark())
    return icon


def logo_svg(icon: Image.Image) -> str:
    """Wrap a generated PNG in SVG for consumers that still expect SVG logos."""
    buffer = io.BytesIO()
    icon.save(buffer, format="PNG")
    encoded = base64.b64encode(buffer.getvalue()).decode("ascii")
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{SIZE}" height="{SIZE}" viewBox="0 0 {SIZE} {SIZE}">
  <title>LocalSend RS</title>
  <image width="{SIZE}" height="{SIZE}" href="data:image/png;base64,{encoded}"/>
</svg>
"""


def icns(icon: pathlib.Path) -> None:
    """Build the classic multi-resolution .icns fallback with iconutil."""
    with tempfile.TemporaryDirectory() as tmp:
        iconset = pathlib.Path(tmp) / "AppIcon.iconset"
        iconset.mkdir()
        with Image.open(icon) as source:
            source = source.convert("RGBA")
            for base in (16, 32, 128, 256, 512):
                for factor in (1, 2):
                    edge = base * factor
                    suffix = "@2x" if factor == 2 else ""
                    name = f"icon_{base}x{base}{suffix}.png"
                    source.resize((edge, edge), Image.Resampling.LANCZOS).save(
                        iconset / name
                    )
        subprocess.run(
            ["iconutil", "-c", "icns", str(iconset), "-o", str(HERE / "AppIcon.icns")],
            check=True,
        )


def compile_catalogue() -> bool:
    """Compile the Icon Composer document into the system appearance-aware assets."""
    with tempfile.TemporaryDirectory() as tmp:
        out = pathlib.Path(tmp) / "out"
        out.mkdir()
        result = subprocess.run(
            [
                "actool",
                "--compile",
                str(out),
                "--platform",
                "macosx",
                "--minimum-deployment-target",
                "26.0",
                "--app-icon",
                "AppIcon",
                "--output-partial-info-plist",
                str(pathlib.Path(tmp) / "partial.plist"),
                str(HERE / "AppIcon.icon"),
            ],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0 or not (out / "Assets.car").exists():
            print("actool could not compile AppIcon.icon; keeping the light icon")
            if result.returncode != 0:
                print(result.stdout[-2000:], result.stderr[-2000:])
            return False
        shutil.copy2(out / "Assets.car", HERE / "Assets.car")
        shutil.copy2(out / "AppIcon.icns", HERE / "AppIcon.icns")
        return True


def icon_composer(assets: dict[str, pathlib.Path]) -> None:
    """Write square mark layers and the light/dark Icon Composer definition."""
    bundle = HERE / "AppIcon.icon"
    (bundle / "Assets").mkdir(parents=True, exist_ok=True)
    for name, path in assets.items():
        shutil.copy2(path, bundle / "Assets" / name)

    def p3(hex_colour: str) -> str:
        r, g, b = (channel / 255 for channel in _rgb(hex_colour))
        return f"display-p3:{r:.5f},{g:.5f},{b:.5f},1.00000"

    light_top, light_bottom = THEMES["light"]["tile"]
    dark_top, dark_bottom = THEMES["dark"]["tile"]
    document = {
        "fill-specializations": [
            {
                "value": {
                    "linear-gradient": [p3(light_top), p3(light_bottom)],
                }
            },
            {
                "appearance": "dark",
                "value": {
                    "linear-gradient": [p3(dark_top), p3(dark_bottom)],
                },
            },
        ],
        "groups": [
            {
                "layers": [
                    {
                        "image-name-specializations": [
                            {"value": "Mark-Light.png"},
                            {"appearance": "dark", "value": "Mark-Dark.png"},
                        ],
                        "name": "Signal",
                        "position": {
                            "scale": 1,
                            "translation-in-points": [0, 0],
                        },
                    }
                ],
                "shadow": {"kind": "neutral", "opacity": 0.35},
                "specular": False,
                "translucency": {"enabled": False, "value": 0.5},
            }
        ],
        "supported-platforms": {"squares": ["macOS"]},
    }
    (bundle / "icon.json").write_text(json.dumps(document, indent=2) + "\n")


def main() -> None:
    generated: dict[str, pathlib.Path] = {}
    mark = render_mark()
    for name in THEMES:
        suffix = name.capitalize()
        icon = render_icon(name, mark)
        icon_path = HERE / f"AppIcon-{suffix}.png"
        icon.save(icon_path)
        generated[name] = icon_path

        mark_path = HERE / f"Mark-{suffix}.png"
        mark.save(mark_path)
        generated[f"mark-{name}"] = mark_path

        (HERE / f"logo-{name}.svg").write_text(logo_svg(icon))

    icns(generated["light"])
    icon_composer(
        {
            "Mark-Light.png": generated["mark-light"],
            "Mark-Dark.png": generated["mark-dark"],
        }
    )
    compiled = compile_catalogue()
    print("wrote logo-*.svg, AppIcon-*.png, Mark-*.png, AppIcon.icns, AppIcon.icon/")
    print("wrote Assets.car" if compiled else "skipped Assets.car (no actool)")


if __name__ == "__main__":
    main()

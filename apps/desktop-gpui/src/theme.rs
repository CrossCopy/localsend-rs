//! Shared shell metrics, appearance helpers, and the live surface preview.

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, Theme, ThemeMode};
use gpui_kit::*;

use crate::service::{DEFAULT_SURFACE_OPACITY, MAX_SURFACE_OPACITY};

pub const SIDEBAR_WIDTH: f32 = 208.;
pub const TOOLBAR_HEIGHT: f32 = 56.;

// --------------------------------------------------------------------- zoom
//
// `Cmd +` / `Cmd -` / `Cmd 0` (`Ctrl` off macOS) scale the whole interface, the
// way every other Mac app does. GPUI resolves `rem()` against the window but our
// panes are written in absolute points, so a zoom has to be applied where the
// numbers are written: [`s`] is the only place a length comes from, and it
// multiplies by the live factor. gpui-kit's own widgets size themselves in rem,
// which `apply_zoom` keeps in step through `Theme::font_size`.

/// The interface scale, in percent. 100 is the size the layout was designed at.
static ZOOM: AtomicU32 = AtomicU32::new(ZOOM_DEFAULT);
static SURFACE_OPACITY_PERCENT: AtomicU8 = AtomicU8::new(DEFAULT_SURFACE_OPACITY);

pub const ZOOM_DEFAULT: u32 = 100;
pub const ZOOM_MIN: u32 = 70;
pub const ZOOM_MAX: u32 = 200;
pub const ZOOM_STEP: u32 = 10;

/// The base font size `apply_macos_theme` uses, in points: 100 % is this size.
const BASE_FONT_SIZE: f32 = 13.;

pub fn zoom_percent() -> u32 {
    ZOOM.load(Ordering::Relaxed)
}

/// The live factor: `1.0` at 100 %.
pub fn zoom() -> f32 {
    zoom_percent() as f32 / 100.
}

/// Clamp, store and report what was actually applied — a zoom that hit the end
/// of its range and silently did nothing is worse than one that says so.
pub fn set_zoom_percent(percent: u32) -> u32 {
    let clamped = percent.clamp(ZOOM_MIN, ZOOM_MAX);
    ZOOM.store(clamped, Ordering::Relaxed);
    clamped
}

/// Set the alpha used by app-drawn background surfaces, returning the clamped
/// percentage. Zero exposes the native window material; 100 is fully opaque.
pub fn set_surface_opacity(percent: u8) -> u8 {
    let applied = percent.min(MAX_SURFACE_OPACITY);
    SURFACE_OPACITY_PERCENT.store(applied, Ordering::Relaxed);
    applied
}

/// Apply the opacity to both app-owned surfaces and GPUI's root surface.
///
/// The root plugin paints `theme.tokens.background` behind the app view. If
/// only the app's own surfaces use alpha, that opaque root layer hides the
/// native blurred window material.
pub fn apply_surface_opacity(percent: u8, cx: &mut App) -> u8 {
    let applied = set_surface_opacity(percent);
    Theme::update(cx, |theme| {
        theme.colors.background = theme.colors.background.alpha(surface_opacity());
    });
    applied
}

pub fn surface_opacity_percent() -> u8 {
    SURFACE_OPACITY_PERCENT.load(Ordering::Relaxed)
}

/// The live alpha factor used by app-owned background surfaces.
pub fn surface_opacity() -> f32 {
    f32::from(surface_opacity_percent()) / 100.
}

pub fn zoom_in() -> u32 {
    set_zoom_percent(zoom_percent() + ZOOM_STEP)
}

pub fn zoom_out() -> u32 {
    set_zoom_percent(zoom_percent() - ZOOM_STEP)
}

actions!(localsend, [ZoomIn, ZoomOut, ZoomReset]);

/// Register the zoom actions and their keys.
///
/// The window bootstrap calls this, and the window tests call it too, so the
/// binding a test presses is the binding that ships. `+` and `=` are both bound
/// because on most layouts `+` *is* shift-`=`, and someone reaching for the plus
/// key should not have to know which one the keymap saw.
pub fn bind_zoom(cx: &mut App) {
    cx.on_action(|_: &ZoomIn, cx| {
        apply_zoom(cx, zoom_percent() + ZOOM_STEP);
    });
    cx.on_action(|_: &ZoomOut, cx| {
        apply_zoom(cx, zoom_percent().saturating_sub(ZOOM_STEP));
    });
    cx.on_action(|_: &ZoomReset, cx| {
        apply_zoom(cx, ZOOM_DEFAULT);
    });
    // `Cmd` on macOS, `Ctrl` on Windows and Linux: the convention on each.
    let prefix = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    cx.bind_keys([
        KeyBinding::new(&format!("{prefix}-="), ZoomIn, None),
        KeyBinding::new(&format!("{prefix}-+"), ZoomIn, None),
        KeyBinding::new(&format!("{prefix}--"), ZoomOut, None),
        KeyBinding::new(&format!("{prefix}-0"), ZoomReset, None),
    ]);
}

pub fn reset_zoom() -> u32 {
    set_zoom_percent(ZOOM_DEFAULT)
}

/// A length in scaled points. Every size, gap, radius and text size in the panes
/// goes through here so that one factor moves all of them together.
pub fn s(value: f32) -> Pixels {
    px(value * zoom())
}

/// Apply a new zoom: keep the app's own metrics in step (they read [`zoom`]) and
/// gpui-kit's rem-based widgets (they read `Theme::font_size` / the window's rem).
pub fn apply_zoom(cx: &mut App, percent: u32) -> u32 {
    let applied = set_zoom_percent(percent);
    Theme::update(cx, |theme| {
        theme.font_size = px(BASE_FONT_SIZE * zoom());
    });
    cx.refresh_windows();
    applied
}

/// An app-owned surface using GPUI Kit's stock background color and live alpha.
///
/// The color arguments remain for source compatibility with the pane renderers;
/// their former product-specific palette is intentionally no longer applied.
pub fn surface(cx: &App, _light: u32, _dark: u32) -> Hsla {
    cx.theme().background.alpha(surface_opacity())
}

pub fn toolbar(cx: &App) -> Hsla {
    cx.theme().title_bar.alpha(surface_opacity())
}
pub fn sidebar(cx: &App) -> Hsla {
    cx.theme().sidebar.alpha(surface_opacity())
}
pub fn card(cx: &App) -> Hsla {
    cx.theme().popover.alpha(surface_opacity())
}
pub fn stripe(cx: &App) -> Hsla {
    cx.theme().list_even.alpha(surface_opacity())
}
pub fn selected(cx: &App) -> Hsla {
    cx.theme().list_active.alpha(surface_opacity())
}
pub fn success(cx: &App) -> Hsla {
    cx.theme().success.alpha(surface_opacity())
}
pub fn warning(cx: &App) -> Hsla {
    cx.theme().warning.alpha(surface_opacity())
}
pub fn danger(cx: &App) -> Hsla {
    cx.theme().danger.alpha(surface_opacity())
}

pub fn apply_macos_theme(mode: ThemeMode, cx: &mut App) {
    Theme::change(mode, None, cx);
    Theme::update(cx, |theme| {
        theme.font_family = ".SystemUIFont".into();
        // Switching appearance must not throw away the person's zoom.
        theme.font_size = px(BASE_FONT_SIZE * zoom());
        theme.colors.background = theme.colors.background.alpha(surface_opacity());
        if !mode.is_dark() {
            // Kit's light success green has insufficient contrast for alert text.
            theme.colors.success = rgb(0x07572c).into();
        }
    });
}

/// Select the native window material from the glass setting.
pub fn window_background_appearance(glass_enabled: bool) -> WindowBackgroundAppearance {
    if glass_enabled {
        WindowBackgroundAppearance::Blurred
    } else {
        WindowBackgroundAppearance::Opaque
    }
}

/// WCAG relative luminance of a color, from its sRGB channels.
///
/// Not the same as the theme's HSL lightness: amber `#C99401` sits at L 0.40 —
/// "dark", so HSL would pick white — yet it carries 7.7:1 against black and only
/// 2.7:1 against white. The weighting is what separates the two.
fn relative_luminance(color: Hsla) -> f32 {
    let rgb = color.to_rgb();
    let linear = |channel: f32| {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(rgb.r) + 0.7152 * linear(rgb.g) + 0.0722 * linear(rgb.b)
}

/// Ink that reads on a filled surface: near-black on a bright fill, near-white
/// on a dark one. The theme's `*_foreground` tokens are not a contrast function —
/// amber in particular ships white text in the light theme, which is what made
/// the pending-offer badge unreadable.
///
/// The threshold is where black and white cross over (equal contrast, 4.6:1);
/// anything above it is bright enough that dark ink is the readable choice.
pub fn ink_on(fill: Hsla) -> Hsla {
    if relative_luminance(fill) > 0.179 {
        black()
    } else {
        white()
    }
}

/// A single-glyph action rendered as a real button rather than a bare icon: a
/// 28 pt square that stays quiet at rest and paints the theme's accent surface
/// under the pointer. Every icon-only action uses this, so a folder, a star and
/// a refresh all answer a hover the way a labelled button does.
///
/// The button's `Size::Size` makes gpui-component scale the glyph to 75 % of the
/// square — 21 pt here, against the 12 pt a `small()` button would use. The
/// square is the pointer target; the glyph inside a Lucide viewbox only inks
/// ~70 % of it, which is why the old 12 pt icons read as specks.
pub fn icon_button(id: impl Into<ElementId>, icon: impl Into<Icon>, cx: &App) -> Button {
    Button::new(id)
        .custom(
            ButtonCustomVariant::new(cx)
                .color(cx.theme().transparent)
                .foreground(cx.theme().muted_foreground)
                .hover(cx.theme().accent)
                .active(cx.theme().accent.opacity(0.75)),
        )
        // `with_size`, not `size`: `size` is the layout box, `with_size` is the
        // button's own scale, and only the latter decides the glyph size.
        .with_size(gpui_kit::component::Size::Size(s(28.)))
        .icon(icon)
}

pub fn theme_toggle_button(cx: &App) -> Button {
    let dark = cx.theme().mode.is_dark();
    icon_button(
        "theme-toggle",
        Icon::new(if dark { IconName::Sun } else { IconName::Moon }),
        cx,
    )
    .accessibility_label("Switch appearance")
    .tooltip(if dark {
        "Switch to light appearance"
    } else {
        "Switch to dark appearance"
    })
    .on_click(|_, _, cx| {
        let mode = if cx.theme().mode.is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        apply_macos_theme(mode, cx);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The factor is process-wide statics, so the tests take a lock and each one
    /// puts it back: thread-parallel tests must not see each other's zoom.
    static ZOOM_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_zoom(percent: u32, test: impl FnOnce()) {
        let _guard = ZOOM_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous = zoom_percent();
        set_zoom_percent(percent);
        test();
        set_zoom_percent(previous);
    }

    #[::core::prelude::v1::test]
    fn lengths_follow_the_zoom_factor() {
        with_zoom(100, || {
            assert_eq!(s(13.), px(13.));
            assert_eq!(zoom(), 1.0);
        });
        with_zoom(150, || {
            assert_eq!(s(13.), px(19.5), "150 % must scale every length by 1.5");
            assert_eq!(s(208.), px(312.));
        });
        with_zoom(70, || {
            assert_eq!(s(10.), px(7.));
        });
    }

    #[::core::prelude::v1::test]
    fn stepping_stops_at_the_ends_instead_of_wrapping() {
        with_zoom(100, || {
            assert_eq!(zoom_in(), 110);
            assert_eq!(zoom_out(), 100);
        });
        with_zoom(ZOOM_MAX, || {
            assert_eq!(zoom_in(), ZOOM_MAX, "zooming past the top must stop");
        });
        with_zoom(ZOOM_MIN, || {
            assert_eq!(zoom_out(), ZOOM_MIN, "zooming past the bottom must stop");
        });
    }

    #[::core::prelude::v1::test]
    fn reset_returns_to_the_designed_size() {
        with_zoom(170, || {
            assert_eq!(reset_zoom(), ZOOM_DEFAULT);
            assert_eq!(zoom(), 1.0);
        });
    }

    #[::core::prelude::v1::test]
    fn surface_opacity_maps_percentage_to_surface_alpha() {
        static OPACITY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = OPACITY_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = surface_opacity_percent();

        assert_eq!(set_surface_opacity(0), 0);
        assert_eq!(surface_opacity(), 0.0);
        assert_eq!(set_surface_opacity(78), 78);
        assert_eq!(surface_opacity(), 0.78);
        assert_eq!(set_surface_opacity(100), 100);
        assert_eq!(surface_opacity(), 1.0);
        assert_eq!(set_surface_opacity(255), 100);

        set_surface_opacity(previous);
    }

    #[::core::prelude::v1::test]
    fn native_window_material_tracks_glass_setting() {
        assert_eq!(
            window_background_appearance(true),
            WindowBackgroundAppearance::Blurred
        );
        assert_eq!(
            window_background_appearance(false),
            WindowBackgroundAppearance::Opaque
        );
    }

    /// The badge is the one place the app paints text on a saturated fill, so
    /// the ink has to be measured rather than inherited from the theme.
    #[::core::prelude::v1::test]
    fn badge_ink_is_readable_on_every_badge_fill() {
        fn contrast(a: Hsla, b: Hsla) -> f32 {
            let (a, b) = (relative_luminance(a), relative_luminance(b));
            (a.max(b) + 0.05) / (a.min(b) + 0.05)
        }

        // Amber as the light theme ships it, near-black for a dark fill.
        for fill in [Hsla::from(rgb(0xC99401)), Hsla::from(rgb(0x18181b))] {
            let ink = ink_on(fill);
            let ratio = contrast(ink, fill);
            assert!(
                ratio >= 4.5,
                "ink {ink:?} on {fill:?} reaches only {ratio:.2}:1"
            );
        }
        assert_eq!(
            ink_on(Hsla::from(rgb(0xC99401))),
            black(),
            "amber needs dark ink"
        );
    }
}

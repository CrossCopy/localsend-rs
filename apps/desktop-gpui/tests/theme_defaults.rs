//! The desktop shell should use GPUI Kit's stock appearance colors.

use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode};
use gpui_kit::TestAppContext;
use localsend_gpui::theme::{self, apply_macos_theme, apply_surface_opacity};

fn rgb(color: gpui_kit::Hsla) -> (f32, f32, f32) {
    let color = color.to_rgb();
    (color.r, color.g, color.b)
}

fn contrast(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    fn luminance((r, g, b): (f32, f32, f32)) -> f32 {
        let linear = |value: f32| {
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    }
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[gpui_kit::test]
fn light_success_text_is_readable_on_light_surfaces(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        apply_macos_theme(ThemeMode::Light, cx);
        let success = rgb(cx.theme().success);
        for background in [(1.0, 1.0, 1.0), (0.76, 0.79, 0.77)] {
            let ratio = contrast(success, background);
            assert!(
                ratio >= 4.5,
                "light success text must reach 4.5:1 contrast, got {ratio:.2}:1"
            );
        }
    });
}

#[gpui_kit::test]
fn dark_success_color_remains_the_kit_default(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        let expected = rgb(cx.theme().success);
        apply_macos_theme(ThemeMode::Light, cx);
        apply_macos_theme(ThemeMode::Dark, cx);
        assert_eq!(rgb(cx.theme().success), expected);
    });
}

#[gpui_kit::test]
fn app_appearance_preserves_stock_theme_colors_and_preview_opacity(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            Theme::change(mode, None, cx);
            let expected = (
                rgb(cx.theme().background),
                rgb(cx.theme().foreground),
                rgb(cx.theme().primary),
                rgb(cx.theme().sidebar),
                rgb(cx.theme().border),
            );
            apply_surface_opacity(78, cx);
            apply_macos_theme(mode, cx);
            let actual = (
                rgb(cx.theme().background),
                rgb(cx.theme().foreground),
                rgb(cx.theme().primary),
                rgb(cx.theme().sidebar),
                rgb(cx.theme().border),
            );
            assert_eq!(
                actual, expected,
                "app must retain the kit's {mode:?} palette"
            );
            assert_eq!(cx.theme().background.a, 0.78);
            assert_eq!(rgb(theme::toolbar(cx)), rgb(cx.theme().title_bar));
            assert_eq!(theme::toolbar(cx).a, 0.78);
            assert_eq!(rgb(theme::sidebar(cx)), rgb(cx.theme().sidebar));
            assert_eq!(theme::sidebar(cx).a, 0.78);

            for percent in [0, 100, 78, 43, 78, 100] {
                apply_surface_opacity(percent, cx);
                assert_eq!(
                    cx.theme().background.a,
                    f32::from(percent) / 100.0,
                    "{mode:?} root alpha must be an absolute preview value at {percent}%"
                );
                assert_eq!(
                    rgb(cx.theme().background),
                    expected.0,
                    "{mode:?} opacity preview must preserve the stock background color"
                );
                assert_eq!(theme::toolbar(cx).a, f32::from(percent) / 100.0);
                assert_eq!(theme::sidebar(cx).a, f32::from(percent) / 100.0);
            }
        }
        apply_surface_opacity(100, cx);
    });
}

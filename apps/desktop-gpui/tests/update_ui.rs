//! The update settings entry point is visible in a detached window, but must not
//! start network work when there is no host runtime behind the app.

use gpui_kit::component::ThemeMode;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    px, size, AppContext as _, Bounds, InputEvent as _, Point, ScrollDelta, ScrollWheelEvent,
    TestAppContext, WindowBounds, WindowOptions,
};
use localsend_gpui::app_state::{DesktopApp, Phase};
use localsend_gpui::theme::apply_macos_theme;

#[gpui_kit::test]
fn settings_show_build_signing_trust_and_detached_check_is_network_free(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        apply_macos_theme(ThemeMode::Light, cx);
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1180.), px(760.)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| DesktopApp::detached(window, cx)),
        )
        .expect("open detached test window")
    });
    cx.update(|cx| {
        view.update(cx, |app, cx| app.select_phase(Phase::Settings, cx));
    });

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.dispatch_event(
            ScrollWheelEvent {
                position: Point {
                    x: px(500.),
                    y: px(500.),
                },
                delta: ScrollDelta::Pixels(Point {
                    x: px(0.),
                    y: px(-1800.),
                }),
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
        assert!(window.try_find("updates-check").is_some());
        let trust = window.find("updates-signing-trust");
        let trust_message = trust.label().unwrap_or_default();
        if option_env!("LOCALSEND_GPUI_TRUSTED_TEAM_ID").is_some() {
            assert!(
                trust_message.contains("pins a publisher Team ID"),
                "unexpected configured trust status: {trust_message}"
            );
        } else {
            assert!(
                trust_message.contains("no pinned publisher Team ID")
                    && trust_message.contains("cannot download, stage, or install"),
                "unexpected unsigned-build status: {trust_message}"
            );
        }
        assert!(
            window.try_find("updates-install-request").is_none(),
            "an unstaged update must not expose an install action"
        );
        window.click("updates-check", cx);
        window.render_frame(cx);
        let status_element = window.find("updates-status");
        let status = status_element.label().unwrap_or_default();
        assert!(
            status.contains("unavailable in a detached session"),
            "unexpected detached update status: {status}"
        );
    })
    .expect("the test window is open");
}

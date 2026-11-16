//! The nearby-device refresh menu is an explicit choice: opening it must not
//! trigger discovery or a subnet scan.

use std::net::Ipv4Addr;
use std::time::SystemTime;

use gpui_kit::component::ThemeMode;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    px, size, AnyWindowHandle, AppContext as _, Bounds, ElementId, Entity, Point, TestAppContext,
    WindowBounds, WindowOptions,
};
use localsend_gpui::app_state::DesktopApp;
use localsend_gpui::service::{Command, NetworkInterface, Peer, PeerKind, ServiceEvent};
use localsend_gpui::theme::apply_macos_theme;
use localsend_rs::Protocol;

struct Harness {
    window: AnyWindowHandle,
    view: Entity<DesktopApp>,
    _serial: std::sync::MutexGuard<'static, ()>,
}

static UI_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Harness {
    fn click(&self, cx: &mut TestAppContext, id: impl Into<ElementId>) {
        let id = id.into();
        cx.update_window(self.window, |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
            window.render_frame(cx);
        })
        .expect("the test window is open");
    }

    fn take_commands(&self, cx: &mut TestAppContext) -> Vec<Command> {
        cx.update(|cx| {
            let store = self.view.read(cx).store().clone();
            store.read(cx).take_commands()
        })
    }

    fn click_menu_item(&self, cx: &mut TestAppContext, index: usize) {
        cx.update_window(self.window, |_, window, cx| {
            window.within("popup-menu").click(index, cx);
        })
        .expect("the test window is open");
    }

    fn feed(&self, cx: &mut TestAppContext, event: ServiceEvent) {
        let store = cx.update(|cx| self.view.read(cx).store().clone());
        cx.update(|cx| store.update(cx, |store, cx| store.apply_event(event, cx)));
    }

    fn press(&self, cx: &mut TestAppContext, key: &str) {
        cx.update_window(self.window, |_, window, cx| {
            window.press(key, cx);
        })
        .expect("the test window is open");
    }
}

fn open(cx: &mut TestAppContext) -> Harness {
    let serial = UI_LOCK.lock().unwrap_or_else(|error| error.into_inner());
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
        .expect("open the test window")
    });
    Harness {
        window,
        view,
        _serial: serial,
    }
}

#[gpui_kit::test]
fn opening_refresh_menu_does_not_start_discovery(cx: &mut TestAppContext) {
    let app = open(cx);
    app.click(cx, "0-0-0");
    app.click(cx, "refresh-peers");

    assert!(
        app.take_commands(cx)
            .iter()
            .all(|command| !matches!(command, Command::RefreshPeers | Command::ScanSubnet { .. })),
        "opening the menu must not start discovery"
    );
    cx.update_window(app.window, |_, window, _| {
        let menu = window.within("popup-menu");
        assert_eq!(menu.find(0usize).label(), Some("Refresh all / multicast"));
    })
    .expect("the test window is open");
}

#[gpui_kit::test]
fn empty_interface_list_is_explained_in_the_refresh_menu(cx: &mut TestAppContext) {
    let app = open(cx);
    app.click(cx, "0-0-0");
    app.click(cx, "refresh-peers");

    cx.update_window(app.window, |_, window, _| {
        assert_eq!(
            window.within("popup-menu").find(2usize).label(),
            Some("No network interfaces available")
        );
    })
    .expect("the test window is open");
}

#[gpui_kit::test]
fn multicast_refresh_remains_an_explicit_menu_action(cx: &mut TestAppContext) {
    let app = open(cx);
    app.click(cx, "0-0-0");
    app.click(cx, "refresh-peers");
    app.press(cx, "down");
    app.press(cx, "enter");

    assert!(
        app.take_commands(cx).contains(&Command::RefreshPeers),
        "choosing the multicast menu item must keep the existing refresh command"
    );
}

#[gpui_kit::test]
fn selecting_an_interface_scans_its_exact_address(cx: &mut TestAppContext) {
    let app = open(cx);
    let selected = Ipv4Addr::new(192, 168, 20, 1);
    app.feed(
        cx,
        ServiceEvent::NetworkInterfacesUpdated(vec![
            NetworkInterface {
                name: "en0".to_string(),
                address: Ipv4Addr::new(192, 168, 10, 1),
            },
            NetworkInterface {
                name: "en1".to_string(),
                address: selected,
            },
        ]),
    );
    app.click(cx, "0-0-0");
    app.click(cx, "refresh-peers");
    let _ = app.take_commands(cx);
    cx.update_window(app.window, |_, window, _| {
        assert_eq!(
            window.within("popup-menu").find(3usize).label(),
            Some("en1 (192.168.20.1)")
        );
    })
    .expect("the test window is open");
    app.click_menu_item(cx, 3);

    assert_eq!(
        app.take_commands(cx),
        vec![Command::ScanSubnet { address: selected }]
    );
}

#[gpui_kit::test]
fn an_active_interface_scan_is_visible_and_keeps_existing_peers(cx: &mut TestAppContext) {
    let app = open(cx);
    let address = Ipv4Addr::new(192, 168, 10, 1);
    app.feed(
        cx,
        ServiceEvent::NetworkInterfacesUpdated(vec![NetworkInterface {
            name: "Wi-Fi".to_string(),
            address,
        }]),
    );
    app.feed(cx, ServiceEvent::PeerFound(peer("pixel", "Pixel 9")));
    app.feed(cx, ServiceEvent::SubnetScanStarted { address });
    let scanning = cx.update(|cx| {
        let store = app.view.read(cx).store().clone();
        store.read(cx).session().scanning_subnets.clone()
    });
    assert!(scanning.contains(&address));
    app.click(cx, "0-0-0");
    cx.run_until_parked();
    let scanning_after_navigation = cx.update(|cx| {
        let store = app.view.read(cx).store().clone();
        store.read(cx).session().scanning_subnets.clone()
    });
    assert!(scanning_after_navigation.contains(&address));

    cx.update_window(app.window, |_, window, _| {
        assert!(window.try_find("peer-pixel").is_some());
        let status = window
            .find("interface-scan-status")
            .label()
            .unwrap_or_default()
            .to_string();
        assert!(status.contains("Scanning Wi-Fi (192.168.10.1)"), "{status}");
    })
    .expect("the test window is open");
}

#[gpui_kit::test]
fn an_active_interface_scan_cannot_be_requested_again(cx: &mut TestAppContext) {
    let app = open(cx);
    let address = Ipv4Addr::new(192, 168, 10, 1);
    app.feed(
        cx,
        ServiceEvent::NetworkInterfacesUpdated(vec![NetworkInterface {
            name: "Wi-Fi".to_string(),
            address,
        }]),
    );
    app.feed(cx, ServiceEvent::SubnetScanStarted { address });
    app.click(cx, "0-0-0");
    app.click(cx, "refresh-peers");
    let _ = app.take_commands(cx);
    app.click_menu_item(cx, 2);

    assert!(
        app.take_commands(cx).is_empty(),
        "the disabled scanning interface must not issue another scan"
    );
}

fn peer(node_id: &str, alias: &str) -> Peer {
    Peer {
        node_id: node_id.to_string(),
        alias: alias.to_string(),
        model: "test".to_string(),
        device_kind: PeerKind::Phone,
        ip: "10.0.0.9".to_string(),
        port: 53317,
        protocol: Protocol::Http,
        last_seen: SystemTime::now(),
    }
}

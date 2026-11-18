//! Window-level tests: the shell renders, the panes switch, an offer can be
//! accepted, a send can be staged, and settings are validated — all against a
//! detached store, so no socket, runtime or file is touched.
//!
//! These are the assertions a screenshot cannot make: that the button a person
//! would click is the one that exists, is enabled, and asks the service for
//! exactly the thing it says it does.
//!
//! `gpui_kit::open_window` wraps the content in a `Root`, so the window handle
//! and the `DesktopApp` entity are kept separately: the window is for frames and
//! clicks, the entity is where the state is read.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gpui_kit::component::{ActiveTheme as _, ThemeMode};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    px, size, AnyWindowHandle, App, AppContext as _, Bounds, Context, ElementId, Entity,
    InputEvent as _, Point, ScrollDelta, ScrollWheelEvent, TestAppContext, WindowBounds,
    WindowOptions,
};
use localsend_gpui::app_state::{DesktopApp, Phase};
use localsend_gpui::service::{
    Command, NoticeLevel, Offer, OfferFile, Peer, PeerKind, ServerStatus, ServiceEvent, Settings,
};
use localsend_gpui::theme::apply_macos_theme;
use localsend_rs::Protocol;

struct Harness {
    window: AnyWindowHandle,
    view: Entity<DesktopApp>,
    /// Every window test holds this for its lifetime: the theme and the zoom
    /// factor are process-wide, so two tests rendering at once would see each
    /// other's scale. The tests are sub-second, so serialising them is free.
    _serial: std::sync::MutexGuard<'static, ()>,
}

static UI_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Harness {
    fn read<R>(&self, cx: &mut TestAppContext, read: impl FnOnce(&DesktopApp, &App) -> R) -> R {
        cx.update(|cx| read(self.view.read(cx), cx))
    }

    fn update<R>(
        &self,
        cx: &mut TestAppContext,
        update: impl FnOnce(&mut DesktopApp, &mut Context<DesktopApp>) -> R,
    ) -> R {
        cx.update(|cx| self.view.update(cx, update))
    }

    fn frame(&self, cx: &mut TestAppContext, f: impl FnOnce(&mut gpui_kit::Window, &mut App)) {
        cx.update_window(self.window, |_, window, cx| {
            window.render_frame(cx);
            f(window, cx);
            window.render_frame(cx);
        })
        .expect("the test window is open");
    }

    fn click(&self, cx: &mut TestAppContext, id: impl Into<ElementId>) {
        let id = id.into();
        self.frame(cx, move |window, cx| window.click(id, cx));
    }

    fn feed(&self, cx: &mut TestAppContext, event: ServiceEvent) {
        let store = self.read(cx, |app, _| app.store().clone());
        cx.update(|cx| store.update(cx, |store, cx| store.apply_event(event, cx)));
    }

    fn phase(&self, cx: &mut TestAppContext) -> Phase {
        self.read(cx, |app, _| app.phase())
    }

    fn take_commands(&self, cx: &mut TestAppContext) -> Vec<Command> {
        self.read(cx, |app, cx| app.store().read(cx).take_commands())
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

fn listening() -> ServiceEvent {
    ServiceEvent::Status(ServerStatus::Listening {
        port: 53317,
        alias: "This Mac".to_string(),
        protocol: Protocol::Http,
        fingerprint: "ab12cd34ef56".to_string(),
        save_dir: PathBuf::from("/tmp/localsend"),
    })
}

fn offer(id: u64) -> ServiceEvent {
    ServiceEvent::Offer(Offer {
        id,
        from: peer("pixel", "Pixel 9"),
        files: vec![OfferFile {
            id: "file-1".to_string(),
            name: "holiday.jpg".to_string(),
            size: 3_400_000,
            mime: "image/jpeg".to_string(),
        }],
        total_bytes: 3_400_000,
    })
}

#[gpui_kit::test]
fn the_shell_opens_on_receive_and_the_sidebar_switches_panes(cx: &mut TestAppContext) {
    let app = open(cx);

    app.frame(cx, |window, _| {
        assert!(window.try_find("nav-send").is_none());
        assert!(window.try_find("nav-receive").is_none());
        assert!(window.try_find("nav-history").is_none());
        assert!(window.try_find("nav-settings").is_none());
        // The receiver starts before the service has said anything.
        let snapshot = window.find("receive-status");
        let status = snapshot.label().unwrap_or_default();
        assert!(
            status.contains("Starting the receiver"),
            "unexpected first screen: {status}"
        );
        assert!(window.try_find(("offer", 1u64)).is_none());
    });

    for (id, expected) in [
        ("0-0-0", Phase::Send),
        ("0-0-2", Phase::History),
        ("0-0-3", Phase::Settings),
        ("0-0-1", Phase::Receive),
    ] {
        app.click(cx, id);
        assert_eq!(app.phase(cx), expected, "clicking {id}");
    }
}

#[gpui_kit::test]
fn a_listening_status_names_the_port_and_the_save_folder(cx: &mut TestAppContext) {
    let app = open(cx);
    app.feed(cx, listening());
    app.frame(cx, |window, _| {
        let snapshot = window.find("receive-status");
        let status = snapshot.label().unwrap_or_default();
        assert!(status.contains("Listening on port 53317"), "{status}");
        assert!(status.contains("HTTP"), "{status}");
        assert!(status.contains("AB12-CD34"), "{status}");
        assert!(status.contains("/tmp/localsend"), "{status}");
    });
}

#[gpui_kit::test]
fn cmd_plus_scales_the_interface_and_zero_puts_it_back(cx: &mut TestAppContext) {
    let app = open(cx);
    cx.update(localsend_gpui::theme::bind_zoom);
    // Another test in this process may have left a factor behind.
    cx.update(|cx| localsend_gpui::theme::apply_zoom(cx, localsend_gpui::theme::ZOOM_DEFAULT));
    app.feed(
        cx,
        ServiceEvent::Status(ServerStatus::Listening {
            port: 53317,
            alias: "Test Device".to_string(),
            protocol: Protocol::Http,
            fingerprint: "abc123def456".to_string(),
            save_dir: PathBuf::from("/tmp/localsend-test"),
        }),
    );

    let measure = |cx: &mut TestAppContext| {
        let mut height = 0.0f32;
        app.frame(cx, |window, _| {
            height = window
                .try_find("receive-status")
                .expect("the status card")
                .bounds()
                .size
                .height
                .into();
        });
        height
    };

    let at_100 = measure(cx);
    assert!(at_100 > 0.0, "the card must have laid out");

    // The shortcut itself, not a direct call: this is what a person presses.
    app.frame(cx, |window, cx| window.press("cmd-=", cx));
    assert_eq!(
        localsend_gpui::theme::zoom_percent(),
        110,
        "Cmd+= must zoom in one step"
    );
    let at_110 = measure(cx);
    assert!(
        at_110 > at_100 * 1.05,
        "the whole card must grow, not just its text: {at_100} -> {at_110}"
    );

    // gpui-kit's own widgets size themselves in rem, so the theme font size is
    // what keeps the buttons in step with the panes.
    cx.update(|cx| {
        let font: f32 = cx.theme().font_size.into();
        assert!(
            (font - 13.0 * 1.1).abs() < 0.01,
            "kit widgets must scale with the panes: {font}"
        );
    });

    app.frame(cx, |window, cx| window.press("cmd--", cx));
    app.frame(cx, |window, cx| window.press("cmd-0", cx));
    assert_eq!(
        localsend_gpui::theme::zoom_percent(),
        100,
        "Cmd+0 must return to the designed size"
    );
    let back = measure(cx);
    assert!(
        (back - at_100).abs() < 0.5,
        "actual size must restore the layout exactly: {at_100} vs {back}"
    );
}

#[gpui_kit::test]
fn an_offer_shows_and_the_accept_button_asks_the_service(cx: &mut TestAppContext) {
    let app = open(cx);
    app.feed(cx, offer(7));

    app.frame(cx, |window, _| {
        assert!(window.try_find(("decline", 7u64)).is_some());
        assert!(window.try_find(("accept", 7u64)).is_some());
        let snapshot = window.find(("offer", 7u64));
        let card = snapshot.label().unwrap_or_default();
        assert!(card.contains("Pixel 9"), "{card}");
    });

    app.click(cx, ("accept", 7u64));
    assert_eq!(
        app.take_commands(cx),
        vec![Command::Accept { id: 7, files: None }]
    );

    // The service echoes the answer back; the prompt must go away.
    app.feed(
        cx,
        ServiceEvent::OfferClosed {
            id: 7,
            accepted: true,
        },
    );
    app.frame(cx, |window, _| {
        assert!(window.try_find(("accept", 7u64)).is_none());
    });
}

#[gpui_kit::test]
fn arriving_offers_raise_a_notification_and_answering_retracts_it(cx: &mut TestAppContext) {
    let app = open(cx);
    app.feed(cx, offer(11));

    // The offer is visible from any pane, not only as a badge in the sidebar.
    app.frame(cx, |window, _| {
        assert!(
            window.try_find(("offer-toast", 11u64)).is_some(),
            "an arriving offer must post a notification"
        );
    });

    // Answering through the ordinary card clears the offer, and the toast goes
    // with it: a notification for an answered request would be stale.
    app.click(cx, ("accept", 11u64));
    app.feed(
        cx,
        ServiceEvent::OfferClosed {
            id: 11,
            accepted: true,
        },
    );
    // Closing a toast runs its exit animation, so let the clock pass the
    // animation before asking the tree whether the card is gone.
    cx.background_executor
        .advance_clock(Duration::from_millis(400));
    cx.run_until_parked();
    app.frame(cx, |window, _| {
        assert!(
            window.try_find(("offer-toast", 11u64)).is_none(),
            "an answered offer must not leave its toast behind"
        );
    });
}

#[gpui_kit::test]
fn the_offer_toast_answers_the_request_without_switching_panes(cx: &mut TestAppContext) {
    let app = open(cx);
    app.feed(cx, offer(13));
    assert_eq!(app.phase(cx), Phase::Receive);

    // The notification is answerable in place: the person does not have to find
    // the pane first, which is the whole point of the card.
    app.click(cx, ("offer-toast-accept", 13u64));
    assert_eq!(
        app.take_commands(cx),
        vec![Command::Accept {
            id: 13,
            files: None
        }]
    );

    cx.background_executor
        .advance_clock(Duration::from_millis(400));
    cx.run_until_parked();
    app.frame(cx, |window, _| {
        assert!(
            window.try_find(("offer-toast", 13u64)).is_none(),
            "answering the toast dismisses it"
        );
    });
}

#[gpui_kit::test]
fn the_icon_actions_are_real_buttons_with_a_pointer_sized_target(cx: &mut TestAppContext) {
    let app = open(cx);

    app.frame(cx, |window, _| {
        for (id, name) in [
            ("toggle-sidebar", "sidebar"),
            ("theme-toggle", "appearance"),
        ] {
            let snapshot = window.find(id);
            assert_eq!(
                snapshot.role(),
                Some(gpui_kit::Role::Button),
                "{name} must be a button, not a bare glyph"
            );
            assert!(
                snapshot.bounds().size.width >= px(24.) && snapshot.bounds().size.height >= px(24.),
                "{name} is {:#?} — too small to aim at",
                snapshot.bounds().size
            );
        }
    });
}

#[gpui_kit::test]
fn declining_an_offer_sends_decline_and_clears_the_prompt(cx: &mut TestAppContext) {
    let app = open(cx);
    app.feed(cx, offer(9));
    app.click(cx, ("decline", 9u64));
    assert_eq!(app.take_commands(cx), vec![Command::Decline { id: 9 }]);
    // The service answers with the same closure event as an accept, so the
    // prompt leaves on confirmation rather than on the click.
    app.feed(
        cx,
        ServiceEvent::OfferClosed {
            id: 9,
            accepted: false,
        },
    );
    app.frame(cx, |window, _| {
        assert!(window.try_find(("decline", 9u64)).is_none());
    });
}

#[gpui_kit::test]
fn receive_progress_renders_a_row_and_later_becomes_history(cx: &mut TestAppContext) {
    let app = open(cx);
    app.feed(
        cx,
        ServiceEvent::ReceiveProgress {
            id: 3,
            sender: "Pixel 9".to_string(),
            file_name: "holiday.jpg".to_string(),
            file_index: 1,
            file_count: 1,
            file_done: 1_700_000,
            file_size: 3_400_000,
            session_done: 1_700_000,
            session_total: 3_400_000,
        },
    );

    let transfers = app.read(cx, |app, cx| {
        app.store()
            .read(cx)
            .session()
            .active_receives()
            .cloned()
            .collect::<Vec<_>>()
    });
    assert_eq!(transfers.len(), 1);
    assert_eq!(transfers[0].peer, "Pixel 9");
    assert!((transfers[0].ratio() - 50.0).abs() < 0.01);

    let path = PathBuf::from("/tmp/localsend/holiday.jpg");
    app.feed(
        cx,
        ServiceEvent::Received {
            id: 3,
            sender: "Pixel 9".to_string(),
            file_name: "holiday.jpg".to_string(),
            path: path.clone(),
            size: 3_400_000,
        },
    );
    app.feed(
        cx,
        ServiceEvent::ReceiveFinished {
            id: 3,
            outcome: localsend_gpui::service::TransferOutcome::Completed,
        },
    );

    let (running, entry) = app.read(cx, |app, cx| {
        let store = app.store().read(cx);
        let session = store.session();
        (
            session.active_receives().next().is_some(),
            session.history.first().cloned(),
        )
    });
    assert!(!running, "a finished receive must leave the live list");
    let entry = entry.expect("a finished receive must land in history");
    assert_eq!(entry.name, "holiday.jpg");
    assert_eq!(entry.path.as_deref(), Some(path.as_path()));
}

#[gpui_kit::test]
fn a_send_can_be_staged_and_started_and_records_the_command(cx: &mut TestAppContext) {
    let app = open(cx);
    let source = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(source.path(), b"hello").unwrap();
    let source_path = source.path().to_path_buf();

    app.feed(cx, ServiceEvent::PeerFound(peer("pixel", "Pixel 9")));
    app.click(cx, "0-0-0");
    app.click(cx, "peer-pixel");

    app.update(cx, |app, cx| app.add_files(vec![source_path.clone()], cx));
    let staged = app.read(cx, |app, _| app.staged().to_vec());
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].size, 5);

    app.click(cx, "send-files");

    let commands = app.take_commands(cx);
    assert_eq!(commands.len(), 1, "{commands:?}");
    match &commands[0] {
        Command::Send { peer, files, pin } => {
            assert_eq!(peer.alias, "Pixel 9");
            assert_eq!(files, &vec![source_path]);
            assert_eq!(pin, &None);
        }
        other => panic!("unexpected command: {other:?}"),
    }
    // Sending consumes the staging list.
    assert!(app.read(cx, |app, _| app.staged().to_vec()).is_empty());
}

#[gpui_kit::test]
fn auto_accept_flips_live_and_apply_validates_the_draft(cx: &mut TestAppContext) {
    let app = open(cx);
    app.click(cx, "0-0-3");

    app.click(cx, "auto-accept-switch");
    assert_eq!(app.take_commands(cx), vec![Command::SetAutoAccept(true)]);

    // Blank the device name and press Apply: the form must refuse, and nothing
    // must reach the service.
    cx.update_window(app.window, |_, window, cx| {
        let form = app.view.read(&*cx).settings_form().clone();
        let alias = form.read(&*cx).alias.clone();
        alias.update(cx, |state, cx| state.set_value("   ", window, cx));
        window.render_frame(cx);
    })
    .expect("the test window is open");

    let view = app.view.clone();
    app.frame(cx, |window, cx| {
        view.update(cx, |app, cx| app.apply_settings(window, cx));
    });

    let error = app.read(cx, |app, cx| app.settings_form().read(cx).error.clone());
    assert!(
        error
            .as_deref()
            .is_some_and(|message| message.contains("name")),
        "expected a name error, got {error:?}"
    );
    assert!(
        app.take_commands(cx).is_empty(),
        "an invalid form must not restart the receiver"
    );
}

#[gpui_kit::test]
fn glass_settings_are_in_the_draft_and_revert_to_applied_values(cx: &mut TestAppContext) {
    let app = open(cx);
    let form = app.read(cx, |app, _| app.settings_form().clone());
    app.click(cx, "0-0-3");

    assert_eq!(
        app.read(cx, |_, cx| cx.theme().background.a),
        0.78,
        "the GPUI root surface should begin at the saved opacity"
    );

    // Slider changes are previewed before Apply, including the GPUI root
    // background used by gpui-component's RootPlugin.
    app.frame(cx, |window, cx| {
        window.dispatch_event(
            ScrollWheelEvent {
                position: Point {
                    x: px(500.),
                    y: px(500.),
                },
                delta: ScrollDelta::Pixels(Point {
                    x: px(0.),
                    y: px(-700.),
                }),
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
    });
    app.frame(cx, |window, cx| {
        let opacity = form.read(cx).surface_opacity.clone();
        opacity.update(cx, |state, cx| {
            state.set_value(43.0, window, cx);
            cx.emit(gpui_kit::component::slider::SliderEvent::Change(
                gpui_kit::component::slider::SliderValue::Single(43.0),
            ));
        });
    });
    assert_eq!(
        localsend_gpui::theme::surface_opacity_percent(),
        43,
        "moving the slider should update the live app surfaces"
    );
    assert_eq!(
        app.read(cx, |_, cx| cx.theme().background.a),
        0.43,
        "the GPUI root layer must not remain opaque over the blurred window"
    );

    app.click(cx, "glass-switch");
    assert_eq!(
        localsend_gpui::theme::surface_opacity_percent(),
        100,
        "disabling glass should preview opaque app surfaces"
    );
    app.click(cx, "settings-reset");
    assert_eq!(
        localsend_gpui::theme::surface_opacity_percent(),
        78,
        "Revert should restore the last applied opacity immediately"
    );

    // Apply keeps the appearance-only change in the app settings, without
    // asking the receiver to restart. Revert then returns to that applied value.
    app.frame(cx, |window, cx| {
        let opacity = form.read(cx).surface_opacity.clone();
        opacity.update(cx, |state, cx| {
            state.set_value(43.0, window, cx);
            cx.emit(gpui_kit::component::slider::SliderEvent::Change(
                gpui_kit::component::slider::SliderValue::Single(43.0),
            ));
        });
    });

    let draft = app.read(cx, |app, cx| {
        app.settings_form()
            .read(cx)
            .draft(&Settings::default(), cx)
            .unwrap()
    });
    assert!(draft.glass_enabled);
    assert_eq!(draft.surface_opacity, 43);

    let view = app.view.clone();
    app.frame(cx, |window, cx| {
        view.update(cx, |app, cx| app.apply_settings(window, cx));
    });
    assert_eq!(
        localsend_gpui::theme::surface_opacity_percent(),
        43,
        "Apply should leave the preview active"
    );
    assert!(
        app.take_commands(cx).is_empty(),
        "visual-only changes must not restart the receiver"
    );
    let apply_error = app.read(cx, |app, cx| app.settings_form().read(cx).error.clone());
    assert!(
        apply_error
            .as_deref()
            .is_some_and(|message| message.contains("only for this session")),
        "a detached app must explain that its visual changes were not persisted: {apply_error:?}"
    );

    app.click(cx, "settings-reset");
    let restored = app.read(cx, |app, cx| {
        app.settings_form()
            .read(cx)
            .draft(&Settings::default(), cx)
            .unwrap()
    });
    assert!(restored.glass_enabled);
    assert_eq!(restored.surface_opacity, 43);
    assert_eq!(localsend_gpui::theme::surface_opacity_percent(), 43);
}

#[gpui_kit::test]
fn a_notice_is_shown_once_and_can_be_dismissed(cx: &mut TestAppContext) {
    let app = open(cx);
    app.feed(
        cx,
        ServiceEvent::Notice {
            level: NoticeLevel::Error,
            message: "Port 53317 is already in use".to_string(),
        },
    );
    app.frame(cx, |window, _| {
        assert!(window.try_find("dismiss-notice").is_some());
    });

    app.click(cx, "dismiss-notice");
    app.frame(cx, |window, _| {
        assert!(window.try_find("dismiss-notice").is_none());
    });
}

#[gpui_kit::test]
fn starring_a_peer_records_a_favorites_change_and_flips_the_button(cx: &mut TestAppContext) {
    let app = open(cx);
    app.feed(cx, ServiceEvent::PeerFound(peer("pixel", "Pixel 9")));
    app.click(cx, "0-0-0");

    app.frame(cx, |window, _| {
        assert!(
            window.try_find("favorite-pixel").is_some(),
            "every peer card needs a star"
        );
    });
    assert!(!app.read(cx, |app, _| app.is_favorite("pixel")));

    app.click(cx, "favorite-pixel");
    assert!(
        app.read(cx, |app, _| app.is_favorite("pixel")),
        "clicking the star must star the device"
    );
    let commands = app.take_commands(cx);
    assert!(
        commands.contains(&Command::SetFavorites(vec!["pixel".to_string()])),
        "the receiver must learn about the new favorite live: {commands:?}"
    );

    // And the same button unstars it.
    app.click(cx, "favorite-pixel");
    assert!(!app.read(cx, |app, _| app.is_favorite("pixel")));
    let commands = app.take_commands(cx);
    assert!(
        commands.contains(&Command::SetFavorites(Vec::new())),
        "un-starring must empty the trusted list: {commands:?}"
    );
}

#[gpui_kit::test]
fn a_stale_peer_is_pruned_from_the_list(cx: &mut TestAppContext) {
    let app = open(cx);
    let mut old = peer("gone", "Old Phone");
    old.last_seen = SystemTime::now() - Duration::from_secs(600);
    app.feed(cx, ServiceEvent::PeerFound(old));
    app.click(cx, "0-0-0");
    app.frame(cx, |window, _| {
        assert!(window.try_find("peer-gone").is_some());
    });

    let store = app.read(cx, |app, _| app.store().clone());
    cx.update(|cx| {
        store.update(cx, |store, cx| {
            assert!(store.session.prune_peers(SystemTime::now()));
            cx.notify();
        });
    });

    app.frame(cx, |window, _| {
        assert!(window.try_find("peer-gone").is_none());
    });
}

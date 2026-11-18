//! Send: pick a discovered device, pick files, watch the bytes leave.

use std::path::PathBuf;
use std::time::SystemTime;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, Sizable as _};
use gpui_kit::*;

use crate::app_state::DesktopApp;
use crate::service::{format, Peer};
use crate::theme::s;
use crate::views::{card, chip, device_icon, file_tile, section_label, transfer_row};

pub fn render(app: &DesktopApp, cx: &mut Context<DesktopApp>) -> AnyElement {
    let session = app.store.read(cx).session().clone();
    let peers = session.peers.clone();
    let selected = app
        .selected_peer
        .as_deref()
        .and_then(|node_id| session.peer(node_id))
        .cloned();
    let active_send = session.active_send().cloned();

    let left = peer_panel(app, &peers, app.selected_peer.as_deref(), &session, cx);
    let mut right = v_flex()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .h_full()
        .gap(s(12.))
        .child(detail_panel(app, selected.as_ref(), cx));
    if let Some(transfer) = active_send {
        right = right.child(
            card(cx)
                .gap(s(10.))
                .child(
                    h_flex()
                        .w_full()
                        .gap(s(10.))
                        .child(section_label("Sending", cx))
                        .child(div().flex_1())
                        .child(chip(
                            format::rate(transfer.bytes_per_second(SystemTime::now())),
                            cx.theme().primary,
                        ))
                        .child(
                            Button::new(("cancel-send", transfer.id))
                                .danger()
                                .xsmall()
                                .label("Cancel")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.store.read(cx).cancel_send(transfer.id);
                                    cx.notify();
                                })),
                        ),
                )
                .child(transfer_row(&transfer, cx))
                .child(
                    div()
                        .text_size(s(11.))
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("to {}", transfer.peer)),
                ),
        );
    }

    // No page title: the window's toolbar already names the pane, and saying it
    // twice ("Send" over "Send files") is the kind of noise a native app avoids.
    // The panes are two full-height columns, so the empty right half is a card
    // that reaches the bottom instead of a band floating above dead space.
    v_flex()
        .flex_1()
        .min_h_0()
        .w_full()
        .p(s(20.))
        .child(
            h_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .items_stretch()
                .gap(s(14.))
                .child(left)
                .child(right),
        )
        .into_any_element()
}

fn peer_panel(
    app: &DesktopApp,
    peers: &[Peer],
    selected: Option<&str>,
    session: &crate::session::Session,
    cx: &mut Context<DesktopApp>,
) -> Div {
    let interfaces = session.network_interfaces.clone();
    let scanning_subnets = session.scanning_subnets.clone();
    let mut list = v_flex().w_full().gap(s(4.));
    if peers.is_empty() {
        list = list.child(
            Empty::new()
                .flex_none()
                .min_h(s(140.))
                .gap(s(8.))
                .p(s(14.))
                .border_0()
                .header(
                    EmptyHeader::new()
                        .gap(s(6.))
                        .media(
                            EmptyMedia::new()
                                .with_variant(EmptyMediaVariant::Icon)
                                .size(s(32.))
                                .mb(s(0.))
                                .child(
                                    Icon::new(IconName::Wifi)
                                        .size(s(22.))
                                        .text_color(cx.theme().muted_foreground),
                                ),
                        )
                        .title(
                            EmptyTitle::new()
                                .text_size(s(12.))
                                .font_weight(FontWeight::MEDIUM)
                                .child("No devices yet"),
                        )
                        .description(
                            EmptyDescription::new()
                                .max_w(s(240.))
                                .text_center()
                                .text_size(s(11.))
                                .child("Both devices need LocalSend open"),
                        ),
                ),
        );
    }
    for peer in peers {
        let active = selected == Some(peer.node_id.as_str());
        let node_id = peer.node_id.clone();
        let name = peer.alias.clone();
        let endpoint = peer.endpoint();
        let kind = peer.device_kind;
        let fingerprint = peer.node_id.clone();
        let favorite = app.is_favorite(&fingerprint);
        let starred = if favorite {
            cx.theme().warning
        } else {
            cx.theme().muted_foreground
        };
        list = list.child(
            ListItem::new(format!("peer-{}", peer.node_id))
                .w_full()
                .px(s(8.))
                .py(s(8.))
                .rounded(s(9.))
                .gap(s(9.))
                .selected(active)
                .accessibility_label(format!("{name} {endpoint}"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_peer(node_id.clone(), cx);
                }))
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap(s(9.))
                        .child(
                            h_flex()
                                .size(s(32.))
                                .flex_shrink_0()
                                .items_center()
                                .justify_center()
                                .rounded(s(8.))
                                .bg(if active {
                                    cx.theme().accent
                                } else {
                                    cx.theme().muted
                                })
                                .child(Icon::new(device_icon(kind)).size(s(16.)).text_color(
                                    if active {
                                        cx.theme().primary
                                    } else {
                                        cx.theme().muted_foreground
                                    },
                                )),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap(s(1.))
                                .child(div().truncate().text_size(s(13.)).child(name))
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(s(10.5))
                                        .text_color(cx.theme().muted_foreground)
                                        .child(endpoint),
                                ),
                        )
                        .child(
                            crate::theme::icon_button(
                                format!("favorite-{}", peer.node_id),
                                Icon::new(IconName::Star).text_color(starred),
                                cx,
                            )
                            .accessibility_label(if favorite {
                                "Remove from favorites"
                            } else {
                                "Add to favorites"
                            })
                            .tooltip(if favorite {
                                "Remove from favorites"
                            } else {
                                "Add to favorites (quick save can accept from it)"
                            })
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.toggle_favorite(&fingerprint, cx);
                                },
                            )),
                        ),
                ),
        );
    }

    let view = cx.entity();
    let mut panel = card(cx)
        .w(s(286.))
        .flex_shrink_0()
        .h_full()
        .gap(s(8.))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap(s(8.))
                .child(section_label("Nearby devices", cx))
                .child(chip(peers.len().to_string(), cx.theme().muted_foreground))
                .child(div().flex_1())
                .child(
                    crate::theme::icon_button("refresh-peers", Icon::new(IconName::RefreshCw), cx)
                        .accessibility_label("Refresh nearby devices")
                        .tooltip("Choose how to refresh nearby devices")
                        .dropdown_menu({
                            let menu_view = view.clone();
                            move |mut menu, _, _| {
                                let multicast_view = menu_view.clone();
                                menu = menu
                                    .item(PopupMenuItem::new("Refresh all / multicast").on_click(
                                        move |_, _, cx| {
                                            multicast_view.update(cx, |this, app_cx| {
                                                this.store.read(app_cx).refresh_peers();
                                            });
                                        },
                                    ))
                                    .separator();

                                if interfaces.is_empty() {
                                    menu.item(
                                        PopupMenuItem::new("No network interfaces available")
                                            .disabled(true),
                                    )
                                } else {
                                    for interface in &interfaces {
                                        let address = interface.address;
                                        let scanning = scanning_subnets.contains(&address);
                                        let label = if scanning {
                                            format!(
                                                "{} ({}) · Scanning…",
                                                interface.name, interface.address
                                            )
                                        } else {
                                            format!("{} ({})", interface.name, interface.address)
                                        };
                                        let interface_view = menu_view.clone();
                                        menu = menu.item(
                                            PopupMenuItem::new(label).disabled(scanning).on_click(
                                                move |_, _, cx| {
                                                    interface_view.update(cx, |this, cx| {
                                                        if !this
                                                            .store
                                                            .read(cx)
                                                            .session()
                                                            .scanning_subnets
                                                            .contains(&address)
                                                        {
                                                            this.store
                                                                .read(cx)
                                                                .scan_selected_interface(address);
                                                        }
                                                    });
                                                },
                                            ),
                                        );
                                    }
                                    menu
                                }
                            }
                        })
                        .on_open_change({
                            let refresh_view = view.clone();
                            move |open, _, cx| {
                                if *open {
                                    refresh_view.update(cx, |this, app_cx| {
                                        this.store.read(app_cx).refresh_network_interfaces();
                                    });
                                }
                            }
                        }),
                ),
        );

    if !session.scanning_subnets.is_empty() {
        let scan_labels = session
            .scanning_subnets
            .iter()
            .map(|address| {
                session
                    .network_interfaces
                    .iter()
                    .find(|interface| interface.address == *address)
                    .map(|interface| format!("{} ({address})", interface.name))
                    .unwrap_or_else(|| address.to_string())
            })
            .collect::<Vec<_>>()
            .join(", ");
        let status = format!("Scanning {scan_labels}…");
        panel = panel.child(
            div()
                .id("interface-scan-status")
                .test_support()
                .aria_label(status.clone())
                .text_size(s(11.))
                .text_color(cx.theme().primary)
                .child(status),
        );
    }

    panel.child(
        v_flex()
            .id("peer-scroll")
            .w_full()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(list),
    )
}

fn detail_panel(app: &DesktopApp, peer: Option<&Peer>, cx: &mut Context<DesktopApp>) -> AnyElement {
    let Some(peer) = peer else {
        return Empty::new()
            .flex_1()
            .min_h_0()
            .gap(s(14.))
            .p(s(28.))
            .header(
                EmptyHeader::new()
                    .gap(s(10.))
                    .media(
                        EmptyMedia::new()
                            .with_variant(EmptyMediaVariant::Icon)
                            .child(
                                Icon::new(IconName::Send)
                                    .size(s(28.))
                                    .text_color(cx.theme().muted_foreground),
                            ),
                    )
                    .title(
                        EmptyTitle::new()
                            .text_size(s(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .child("Choose a device"),
                    )
                    .description(
                        EmptyDescription::new()
                            .max_w(s(360.))
                            .text_center()
                            .text_size(s(12.))
                            .child(
                                "Pick a device on the left, add the files you want to send, then press Send.",
                            ),
                    ),
            )
            .into_any_element();
    };

    let mut files = v_flex().w_full().gap(s(4.));
    if app.staged.is_empty() {
        files = files.child(
            div()
                .py(s(10.))
                .text_size(s(12.))
                .text_color(cx.theme().muted_foreground)
                .child("No files chosen yet."),
        );
    }
    for (index, file) in app.staged.iter().enumerate() {
        let path = file.path.clone();
        let name = file.name.clone();
        let size = format::bytes(file.size);
        files = files.child(
            div()
                .w_full()
                .px(s(9.))
                .py(s(6.))
                .rounded(s(6.))
                .bg(cx.theme().list_even)
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap(s(9.))
                        .child(file_tile(&name, cx))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap(s(1.))
                                .child(div().truncate().text_size(s(12.)).child(name))
                                .child(
                                    div()
                                        .text_size(s(10.5))
                                        .text_color(cx.theme().muted_foreground)
                                        .child(size),
                                ),
                        )
                        .child(
                            crate::theme::icon_button(
                                ("unstage", index),
                                Icon::new(IconName::X),
                                cx,
                            )
                            .accessibility_label("Remove from the send")
                            .tooltip(path.display().to_string())
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.unstage(index, cx);
                                },
                            )),
                        ),
                ),
        );
    }

    let total: u64 = app.staged.iter().map(|file| file.size).sum();
    let can_send = !app.staged.is_empty();
    let pin_value = app.pin_input.read(cx).value().to_string();

    card(cx)
        .flex_1()
        .min_w_0()
        .min_h_0()
        .gap(s(12.))
        .child(
            h_flex()
                .w_full()
                .gap(s(9.))
                .child(
                    Icon::new(device_icon(peer.device_kind))
                        .size(s(18.))
                        .text_color(cx.theme().primary),
                )
                .child(
                    v_flex()
                        .min_w_0()
                        .gap(s(1.))
                        .child(
                            div()
                                .truncate()
                                .text_size(s(14.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(peer.alias.clone()),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(s(11.))
                                .text_color(cx.theme().muted_foreground)
                                .child(peer.endpoint()),
                        ),
                )
                .child(div().flex_1())
                .child(chip(
                    peer.protocol.as_str().to_uppercase(),
                    cx.theme().primary,
                )),
        )
        .child(div().h(s(1.)).w_full().bg(cx.theme().border))
        .child(
            h_flex()
                .w_full()
                .gap(s(8.))
                .child(section_label("Files", cx))
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(s(11.))
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "{} · {}",
                            format::files(app.staged.len()),
                            format::bytes(total)
                        )),
                ),
        )
        .child(files)
        .child(
            crate::views::labelled_icon_button("add-files", IconName::Plus, "Add files…")
                .ghost()
                .w_full()
                .on_click(cx.listener(|this, _, _, cx| this.pick_files(cx))),
        )
        .child(div().flex_1())
        .child(
            v_flex()
                .w_full()
                .gap(s(8.))
                .child(section_label("PIN", cx))
                .child(
                    gpui_kit::component::input::Input::new(&app.pin_input)
                        .aria_label("Receiver PIN")
                        .cleanable(true),
                )
                .child(
                    div()
                        .text_size(s(11.))
                        .text_color(cx.theme().muted_foreground)
                        .child(if pin_value.is_empty() {
                            "Only needed if the receiving device shows one."
                        } else {
                            "Will be sent with the transfer request."
                        }),
                ),
        )
        .child(
            crate::views::labelled_icon_button(
                "send-files",
                IconName::Send,
                if can_send {
                    format!("Send {} to {}", format::files(app.staged.len()), peer.alias)
                } else {
                    "Send".to_string()
                },
            )
            .primary()
            .w_full()
            .h(s(32.))
            .disabled(!can_send)
            .on_click(cx.listener(|this, _, _, cx| this.start_send(cx))),
        )
        .into_any_element()
}

/// File names are read once when staged, so a file that changes on disk is still
/// shown with the size the send will see.
pub fn staged_files(paths: Vec<PathBuf>) -> Vec<crate::service::SendFile> {
    paths
        .into_iter()
        .filter_map(|path| {
            let metadata = std::fs::metadata(&path).ok()?;
            if !metadata.is_file() {
                return None;
            }
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            Some(crate::service::SendFile {
                name,
                size: metadata.len(),
                path,
            })
        })
        .collect()
}

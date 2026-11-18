//! Receive: the receiver's own status, the offers waiting for an answer, live
//! progress and the files that just landed.

use gpui_kit::assets::IconName;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Icon, Sizable as _};
use gpui_kit::*;
use std::time::SystemTime;

use crate::app_state::DesktopApp;
use crate::service::{format, Offer, ServerStatus};
use crate::theme::s;
use crate::views::{card, chip, file_tile, reveal_button, section_label, transfer_row};

pub fn render(app: &DesktopApp, cx: &mut Context<DesktopApp>) -> AnyElement {
    let session = app.store.read(cx).session().clone();
    let status = session.status.clone();
    let offers = session.offers.clone();
    let active: Vec<_> = session.active_receives().cloned().collect();
    let recent: Vec<_> = session.received_files().take(10).cloned().collect();

    let mut content = v_flex()
        .flex_1()
        .min_h_0()
        .w_full()
        .p(s(20.))
        .gap(s(14.))
        .child(status_card(&status, cx));

    if !offers.is_empty() {
        content = content.child(
            h_flex()
                .w_full()
                .gap(s(8.))
                .child(section_label("Waiting for your answer", cx))
                .child(chip(offers.len().to_string(), cx.theme().warning)),
        );
        for offer in &offers {
            content = content.child(offer_card(offer, cx));
        }
    }

    if !active.is_empty() {
        content = content.child(section_label("Receiving now", cx));
        for transfer in &active {
            content = content.child(
                card(cx)
                    .gap(s(8.))
                    .child(
                        h_flex()
                            .w_full()
                            .gap(s(8.))
                            .child(
                                Icon::new(IconName::ArrowDown)
                                    .size(s(14.))
                                    .text_color(cx.theme().primary),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(s(12.))
                                    .child(format!("from {}", transfer.peer)),
                            ),
                    )
                    .child(transfer_row(transfer, cx)),
            );
        }
    }

    if !recent.is_empty() {
        content = content.child(section_label("Recently received", cx));
        let mut rows = card(cx).p(s(4.)).gap(s(0.));
        for (index, entry) in recent.iter().enumerate() {
            let id = entry.id;
            let name = entry.summary();
            let path = entry.path.clone();
            let peer = entry.peer.clone();
            let size = format::bytes(entry.size);
            let when = format::time_ago(entry.at, SystemTime::now());
            rows = rows.child(
                div()
                    .w_full()
                    .px(s(8.))
                    .py(s(5.))
                    .rounded(s(6.))
                    .bg(if index % 2 == 0 {
                        cx.theme().list_even
                    } else {
                        cx.theme().background
                    })
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
                                            .truncate()
                                            .text_size(s(10.5))
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!("{peer} · {size} · {when}")),
                                    ),
                            )
                            .children(
                                path.clone()
                                    .map(|path| reveal_button(("reveal", id), path, cx)),
                            ),
                    ),
            );
        }
        content = content.child(rows);
    }

    if offers.is_empty() && active.is_empty() && recent.is_empty() {
        content = content.child(receive_empty_state(cx));
    }

    content.into_any_element()
}

/// The idle receive pane. A receiver that is waiting is not an empty screen — it
/// is doing the thing the app exists for — so it pairs the kit's empty state with
/// three short setup steps rather than an apology.
fn receive_empty_state(cx: &mut Context<DesktopApp>) -> impl IntoElement {
    let steps = [
        ("1", "Open LocalSend on the other device"),
        ("2", "Pick this device in its list"),
        ("3", "Say yes here — files land in your folder"),
    ];
    let mut step_row = h_flex().w_full().max_w(s(600.)).gap(s(10.)).items_stretch();
    for (number, text) in steps {
        step_row = step_row.child(
            // A numbered step, not a badge: the number sits in a circle of its own
            // size above a centred sentence, so the three read as a sequence.
            v_flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap(s(8.))
                .px(s(12.))
                .py(s(12.))
                .rounded(s(10.))
                .bg(cx.theme().list_even)
                .child(
                    h_flex()
                        .size(s(22.))
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(cx.theme().accent)
                        .text_size(s(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(cx.theme().accent_foreground)
                        .child(number),
                )
                .child(
                    div()
                        .text_center()
                        .text_size(s(11.5))
                        .text_color(cx.theme().muted_foreground)
                        .child(text),
                ),
        );
    }

    Empty::new()
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
                            Icon::new(IconName::ArrowDownToLine)
                                .size(s(18.))
                                .text_color(cx.theme().primary),
                        ),
                )
                .title(
                    EmptyTitle::new()
                        .text_size(s(16.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Ready to receive"),
                )
                .description(
                    EmptyDescription::new()
                        .max_w(s(420.))
                        .text_center()
                        .text_size(s(12.))
                        .child(
                            "Nothing has arrived yet. When another device offers files, they wait \
                             here for your answer before anything touches your disk.",
                        ),
                ),
        )
        .child(step_row)
}

fn status_card(status: &ServerStatus, cx: &mut Context<DesktopApp>) -> impl IntoElement {
    let (alert, title, detail, listening) = match status {
        ServerStatus::Starting => (
            Alert::warning(
                "receive-status-alert",
                "Opening the port and the TLS identity.",
            )
            .title("Starting the receiver…")
            .icon(Icon::new(IconName::Radio).size(s(22.))),
            "Starting the receiver…".to_string(),
            "Opening the port and the TLS identity.".to_string(),
            false,
        ),
        ServerStatus::Listening {
            port,
            alias,
            protocol,
            fingerprint,
            save_dir,
        } => (
            Alert::success(
                "receive-status-alert",
                format!(
                    "{alias} · {} · {} · saving to {}",
                    protocol.as_str().to_uppercase(),
                    format::short_fingerprint(fingerprint),
                    save_dir.display()
                ),
            )
            .title(format!("Listening on port {port}"))
            .icon(Icon::new(IconName::Radio).size(s(22.))),
            format!("Listening on port {port}"),
            format!(
                "{alias} · {} · {} · saving to {}",
                protocol.as_str().to_uppercase(),
                format::short_fingerprint(fingerprint),
                save_dir.display()
            ),
            true,
        ),
        ServerStatus::Failed { message } => (
            Alert::error("receive-status-alert", message.clone())
                .title("The receiver could not start")
                .icon(Icon::new(IconName::OctagonAlert).size(s(22.))),
            "The receiver could not start".to_string(),
            message.clone(),
            false,
        ),
        ServerStatus::Stopped => (
            Alert::new("receive-status-alert", "Open Settings to start it again.")
                .title("The receiver is stopped")
                .icon(Icon::new(IconName::OctagonAlert).size(s(22.))),
            "The receiver is stopped".to_string(),
            "Open Settings to start it again.".to_string(),
            false,
        ),
    };

    let mut card = v_flex()
        .w_full()
        .gap(s(8.))
        .id("receive-status")
        .test_support()
        .aria_label(format!("{title}. {detail}"))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap(s(8.))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(alert.px(s(16.)).py(s(10.)).gap(s(12.)).rounded(s(12.))),
                )
                .children(listening.then(|| {
                    h_flex()
                        .items_center()
                        .gap(s(6.))
                        .child(chip("ONLINE", cx.theme().success))
                        .child(
                            crate::theme::icon_button(
                                "open-save-folder",
                                Icon::new(IconName::FolderOpen),
                                cx,
                            )
                            .accessibility_label("Show the received-files folder")
                            .tooltip("Show the folder received files go to")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let ServerStatus::Listening { save_dir, .. } =
                                    &this.store.read(cx).session().status
                                {
                                    crate::views::reveal_in_file_manager(save_dir);
                                }
                            })),
                        )
                })),
        );

    if matches!(status, ServerStatus::Failed { .. } | ServerStatus::Stopped) {
        card = card.child(
            crate::views::labelled_icon_button(
                "status-open-settings",
                IconName::Settings,
                "Open Settings",
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.select_phase(crate::app_state::Phase::Settings, cx)
            })),
        );
    }
    card
}

fn offer_card(offer: &Offer, cx: &mut Context<DesktopApp>) -> impl IntoElement {
    let id = offer.id;
    let sender = offer.from.alias.clone();
    let mut files = v_flex().w_full().gap(s(3.));
    for file in offer.files.iter().take(6) {
        let name = file.name.clone();
        let size = format::bytes(file.size);
        files = files.child(
            h_flex()
                .w_full()
                .min_w_0()
                .gap(s(9.))
                .child(file_tile(&name, cx))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(s(12.))
                        .child(name),
                )
                .child(
                    div()
                        .text_size(s(11.))
                        .text_color(cx.theme().muted_foreground)
                        .child(size),
                ),
        );
    }
    if offer.files.len() > 6 {
        files = files.child(
            div()
                .text_size(s(11.))
                .text_color(cx.theme().muted_foreground)
                .child(format!("… and {} more", offer.files.len() - 6)),
        );
    }

    card(cx)
        .id(("offer", id))
        .test_support()
        .aria_label(format!("Offer from {sender}"))
        .gap(s(10.))
        .child(
            h_flex()
                .w_full()
                .gap(s(9.))
                .child(
                    Icon::new(IconName::CircleUser)
                        .size(s(17.))
                        .text_color(cx.theme().primary),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(s(1.))
                        .child(
                            div()
                                .truncate()
                                .text_size(s(13.5))
                                .font_weight(FontWeight::MEDIUM)
                                .child(format!(
                                    "{sender} wants to send you {}",
                                    format::files(offer.file_count())
                                )),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(s(11.))
                                .text_color(cx.theme().muted_foreground)
                                .child(offer.from.endpoint()),
                        ),
                )
                .child(chip(
                    format::bytes(offer.total_bytes),
                    cx.theme().muted_foreground,
                )),
        )
        .child(files)
        .child(
            h_flex()
                .w_full()
                .justify_end()
                .gap(s(8.))
                .child(
                    Button::new(("decline", id))
                        .ghost()
                        .small()
                        .label("Decline")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store.read(cx).decline(id);
                            cx.notify();
                        })),
                )
                .child(
                    crate::views::labelled_icon_button(("accept", id), IconName::Check, "Accept")
                        .primary()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store.read(cx).accept(id);
                            cx.notify();
                        })),
                ),
        )
}

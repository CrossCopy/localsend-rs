//! History: what arrived and what left, newest first, with the failures named.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, Sizable as _};
use gpui_kit::*;
use std::time::{Duration, SystemTime};

use crate::app_state::DesktopApp;
use crate::service::{format, TransferOutcome};
use crate::session::{Direction, HistoryEntry};
use crate::theme::s;
use crate::views::{card, chip, file_tile, reveal_button};

pub fn render(app: &DesktopApp, cx: &mut Context<DesktopApp>) -> AnyElement {
    let session = app.store.read(cx).session().clone();
    let entries = session.history.clone();

    // The toolbar already says "History"; this line only carries what is specific
    // to this list (how much of it there is) and the one action it has.
    let header = h_flex()
        .w_full()
        .items_center()
        .gap(s(10.))
        .child(
            div()
                .text_size(s(12.))
                .text_color(cx.theme().muted_foreground)
                .child(if entries.is_empty() {
                    "Newest first".to_string()
                } else {
                    format!("{} · newest first", format::files(entries.len()))
                }),
        )
        .child(div().flex_1())
        .child(
            Button::new("clear-history")
                .ghost()
                .small()
                .icon(Icon::new(IconName::Delete).small())
                .label("Clear")
                .tooltip("Clear this list (files on disk are untouched)")
                .disabled(entries.is_empty())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.store.update(cx, |store, cx| {
                        store.session.clear_history();
                        cx.notify();
                    });
                    cx.notify();
                })),
        );

    // A list of file rows measured to a readable width, centred: at window width
    // a 12 pt row of text and a chip drift far apart.
    let mut column = v_flex()
        .flex_1()
        .min_h_0()
        .w_full()
        .max_w(s(880.))
        .gap(s(14.))
        .child(header);

    if entries.is_empty() {
        column = column.child(
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
                                    Icon::new(IconName::Clock)
                                        .size(s(28.))
                                        .text_color(cx.theme().muted_foreground),
                                ),
                        )
                        .title(
                            EmptyTitle::new()
                                .text_size(s(14.))
                                .font_weight(FontWeight::MEDIUM)
                                .child("Nothing yet"),
                        )
                        .description(
                            EmptyDescription::new()
                                .max_w(s(360.))
                                .text_center()
                                .text_size(s(12.))
                                .child(
                                    "Finished transfers appear here, one row per file, with the reason when something failed.",
                                ),
                        ),
                ),
        );
    } else {
        let mut rows = v_flex().w_full().gap(s(10.));
        let mut last_day: Option<String> = None;
        let mut day_rows = card(cx).p(s(4.));
        for (index, entry) in entries.iter().enumerate() {
            let day = day_label(entry.at, SystemTime::now());
            if last_day.as_deref() != Some(day.as_str()) {
                if last_day.is_some() {
                    rows = rows.child(day_rows);
                    day_rows = card(cx).p(s(4.));
                }
                let heading = day.clone();
                day_rows = day_rows.child(
                    div()
                        .px(s(9.))
                        .pt(s(5.))
                        .pb(s(4.))
                        .text_size(s(10.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(cx.theme().muted_foreground)
                        .child(heading),
                );
                last_day = Some(day);
            }
            let path = entry.path.clone();
            day_rows = day_rows.child(history_row(entry, index, path, cx));
        }
        rows = rows.child(day_rows);
        column = column.child(
            v_flex().flex_1().min_h_0().w_full().child(
                div()
                    .id("history-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(rows),
            ),
        );
    }

    v_flex()
        .flex_1()
        .min_h_0()
        .w_full()
        .p(s(20.))
        .items_center()
        .child(column)
        .into_any_element()
}

/// `Today`, `Yesterday`, or a date. Used to break the history into days, which
/// is how people remember transfers ("the thing I got yesterday").
fn day_label(then: SystemTime, now: SystemTime) -> String {
    let elapsed = now.duration_since(then).unwrap_or(Duration::ZERO).as_secs();
    match elapsed {
        0..=86_399 => "Today".to_string(),
        86_400..=172_799 => "Yesterday".to_string(),
        _ => {
            let days = elapsed / 86_400;
            format!("{days} days ago")
        }
    }
}

fn history_row(
    entry: &HistoryEntry,
    index: usize,
    path: Option<std::path::PathBuf>,
    cx: &mut Context<DesktopApp>,
) -> Div {
    let incoming = entry.direction == Direction::Incoming;
    let peer = if incoming {
        format!("from {}", entry.peer)
    } else {
        format!("to {}", entry.peer)
    };
    let when = format::time_ago(entry.at, SystemTime::now());
    let size = format::bytes(entry.size);

    let outcome = match &entry.outcome {
        TransferOutcome::Completed => None,
        TransferOutcome::Failed { message } => Some((message.clone(), cx.theme().danger)),
        TransferOutcome::Cancelled => Some(("Cancelled".to_string(), cx.theme().warning)),
    };

    let mut summary = h_flex()
        .w_full()
        .min_w_0()
        .gap(s(9.))
        .child(file_tile(&entry.summary(), cx))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(s(1.))
                .child(div().truncate().text_size(s(12.5)).child(entry.summary()))
                .child(
                    div()
                        .truncate()
                        .text_size(s(10.5))
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("{peer} · {size} · {when}")),
                ),
        )
        .child(chip(
            if incoming { "RECEIVED" } else { "SENT" },
            if incoming {
                cx.theme().primary
            } else {
                cx.theme().muted_foreground
            },
        ));
    if let Some((message, color)) = outcome {
        summary = summary.child(
            div()
                .max_w(s(260.))
                .truncate()
                .text_size(s(10.5))
                .text_color(color)
                .child(message),
        );
    }
    if let Some(path) = path {
        summary = summary.child(reveal_button(("history-reveal", index), path));
    }

    div()
        .w_full()
        .px(s(9.))
        .py(s(6.))
        .rounded(s(6.))
        .bg(if index.is_multiple_of(2) {
            cx.theme().list_even
        } else {
            cx.theme().background
        })
        .child(summary)
}

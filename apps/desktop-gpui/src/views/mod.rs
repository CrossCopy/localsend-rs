//! The four screens and the pieces they share.
//!
//! A view is a pure function of the store snapshot plus the shell's own UI state
//! (which peer is selected, what is staged). Nothing here owns transfer state, so
//! a view can be re-rendered at any time without losing an offer or a progress
//! row — those live in [`crate::store::TransferStore`].

pub mod history;
pub mod receive;
pub mod send;
pub mod settings;

use std::time::SystemTime;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Icon, Sizable as _};
use gpui_kit::*;

use crate::service::{format, PeerKind};
use crate::session::Transfer;
use crate::theme::s;

/// The glyph that stands for a device kind.
pub fn device_icon(kind: PeerKind) -> IconName {
    match kind {
        PeerKind::Phone => IconName::Smartphone,
        PeerKind::Computer => IconName::Laptop,
        PeerKind::Server => IconName::Server,
    }
}

/// The glyph that stands for a file, by its extension. A list of names all
/// reading "document" tells a person nothing about what they are about to take;
/// an image, a video and a zip are distinguishable at a glance.
pub fn file_icon(name: &str) -> IconName {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "svg" | "bmp" | "tiff" => {
            IconName::FileImage
        }
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" => IconName::FilePlay,
        "mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" => IconName::FileMusic,
        "zip" | "tar" | "gz" | "7z" | "rar" | "xz" | "bz2" => IconName::FileArchive,
        "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "c" | "h" | "cpp" | "swift" | "kt"
        | "java" | "sh" | "toml" | "json" | "yml" | "yaml" => IconName::FileCode,
        "csv" | "xls" | "xlsx" | "numbers" => IconName::FileChartColumn,
        "pdf" | "doc" | "docx" | "txt" | "md" | "rtf" | "pages" => IconName::FileText,
        _ => IconName::File,
    }
}

/// A tinted square tile holding a file's kind icon: the shape a file row leads
/// with, so a list of names scans as kinds rather than as text.
pub fn file_tile(name: &str, cx: &App) -> Div {
    h_flex()
        .size(s(30.))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded(s(7.))
        .bg(cx.theme().muted)
        .child(
            Icon::new(file_icon(name))
                .size(s(18.))
                .text_color(cx.theme().muted_foreground),
        )
}

/// A grey uppercase heading above a group.
pub fn section_label(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_size(s(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// The explanation an empty pane gets: what would be here, and why it is not.
pub fn empty_state(
    icon: IconName,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    cx: &App,
) -> impl IntoElement {
    Empty::new()
        .flex_1()
        .min_h_0()
        .gap(s(10.))
        .p(s(28.))
        .border_0()
        .header(
            EmptyHeader::new()
                .gap(s(10.))
                .media(
                    EmptyMedia::new()
                        .with_variant(EmptyMediaVariant::Icon)
                        .size(s(36.))
                        .mb(s(0.))
                        .child(
                            Icon::new(icon)
                                .size(s(28.))
                                .text_color(cx.theme().muted_foreground),
                        ),
                )
                .title(
                    EmptyTitle::new()
                        .text_size(s(14.))
                        .font_weight(FontWeight::MEDIUM)
                        .child(title.into()),
                )
                .description(
                    EmptyDescription::new()
                        .max_w(s(360.))
                        .text_center()
                        .text_size(s(12.))
                        .child(detail.into()),
                ),
        )
}

/// One transfer row: what is moving, how far, how fast.
pub fn transfer_row(transfer: &Transfer, cx: &App) -> Div {
    let now = SystemTime::now();
    let file = transfer
        .current_file()
        .map(|file| file.name.clone())
        .unwrap_or_else(|| "Waiting for the sender".to_string());
    let rate = format::rate(transfer.bytes_per_second(now));
    let eta = transfer
        .eta(now)
        .map(|eta| format!(" · {eta} left"))
        .unwrap_or_default();
    let counts = if transfer.files.len() > 1 {
        format!(
            "file {} of {} · ",
            transfer.file_index.max(1),
            transfer.files.len()
        )
    } else {
        String::new()
    };
    v_flex()
        .w_full()
        .gap(s(6.))
        .child(
            h_flex()
                .w_full()
                .gap(s(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(s(13.))
                        .child(file),
                )
                .child(
                    div()
                        .text_size(s(11.))
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("{:.0}%", transfer.ratio())),
                ),
        )
        .child(
            gpui_kit::component::progress::Progress::new(("transfer", transfer.id))
                .value(transfer.ratio())
                .h(s(5.)),
        )
        .child(
            div()
                .text_size(s(11.))
                .text_color(cx.theme().muted_foreground)
                .child(format!(
                    "{counts}{} of {} · {rate}{eta}",
                    format::bytes(transfer.session_done),
                    format::bytes(transfer.session_total),
                )),
        )
}

/// A compact status tag for counts, protocol labels, and transfer state.
pub fn chip(label: impl Into<SharedString>, color: Hsla) -> Tag {
    let label = label.into();
    Tag::custom(color.opacity(0.16), color, color.opacity(0.32))
        .flex_none()
        .h(s(18.))
        .px(s(7.))
        .py(s(0.))
        .rounded_full()
        .text_size(s(10.5))
        .font_weight(FontWeight::MEDIUM)
        .child(label)
}

/// The little status dot: green listening, amber starting, red failed.
pub fn status_dot(color: Hsla) -> Div {
    div().size(s(8.)).rounded_full().bg(color)
}

/// The one card surface every pane uses.
pub fn card(cx: &App) -> Div {
    v_flex()
        .bg(cx.theme().background)
        .border_1()
        .border_color(cx.theme().border)
        .rounded(s(9.))
        .p(s(14.))
}

/// A labelled button whose glyph is sized to the text rather than to the
/// button's built-in icon slot (which is 12 pt even at `small()`). Written as a
/// child row so the icon follows the app zoom and stays legible next to the
/// label; the explicit accessibility label replaces the one `.label` would set.
pub fn labelled_icon_button(
    id: impl Into<ElementId>,
    icon: IconName,
    label: impl Into<SharedString>,
) -> Button {
    let label = label.into();
    Button::new(id)
        .small()
        .accessibility_label(label.clone())
        .child(
            h_flex()
                .gap(s(6.))
                .child(Icon::new(icon).size(s(17.)))
                .child(div().child(label)),
        )
}

/// The reveal-in-Finder icon, shared by receive and history.
pub fn reveal_button(id: impl Into<ElementId>, path: std::path::PathBuf, cx: &App) -> Button {
    crate::theme::icon_button(id, Icon::new(IconName::FolderOpen), cx)
        .tooltip(format!("Show in Finder — {}", path.display()))
        .on_click(move |_, _, _| reveal_in_file_manager(&path))
}

/// Show a received file where the platform shows files: Finder's column for it.
pub fn reveal_in_file_manager(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer")
        .arg("/select,")
        .arg(path)
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let parent = path.parent().unwrap_or(path);
        let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
    }
}

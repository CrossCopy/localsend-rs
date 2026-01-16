//! Popup overlays for transfer confirmations and status messages.

use crate::protocol::{DeviceInfo, FileMetadata};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use std::collections::HashMap;
use tokio::sync::oneshot;

use super::theme::THEME;

/// Types of popup overlays.
#[allow(dead_code)]
pub enum Popup {
    /// Confirmation dialog for incoming transfer request.
    TransferConfirm {
        sender: DeviceInfo,
        files: HashMap<String, FileMetadata>,
        response_tx: oneshot::Sender<bool>,
    },
    /// Progress indicator for active transfer.
    TransferProgress {
        file_name: String,
        received: u64,
        total: u64,
    },
    /// Simple message popup (success/error/info).
    Message { text: String, level: MessageLevel },
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum MessageLevel {
    Success,
    Error,
    Info,
}

impl Popup {
    /// Render the popup as an overlay.
    pub fn render(&self, frame: &mut ratatui::Frame) {
        let area = frame.area();

        // Calculate centered popup area (60% width, varying height)
        let popup_area = centered_rect(60, 30, area);

        // Clear the area behind the popup
        frame.render_widget(Clear, popup_area);

        match self {
            Popup::TransferConfirm { sender, files, .. } => {
                self.render_transfer_confirm(frame, popup_area, sender, files);
            }
            Popup::TransferProgress {
                file_name,
                received,
                total,
            } => {
                self.render_progress(frame, popup_area, file_name, *received, *total);
            }
            Popup::Message { text, level } => {
                self.render_message(frame, popup_area, text, *level);
            }
        }
    }

    fn render_transfer_confirm(
        &self,
        frame: &mut ratatui::Frame,
        area: Rect,
        sender: &DeviceInfo,
        files: &HashMap<String, FileMetadata>,
    ) {
        let block = Block::default()
            .title(" 📥 Incoming Transfer ")
            .title_style(THEME.popup_title)
            .borders(Borders::ALL)
            .border_style(THEME.popup_border);

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let file_count = files.len();
        let total_size: u64 = files.values().map(|f| f.size).sum();
        let size_str = format_size(total_size);

        let file_list: Vec<String> = files
            .values()
            .take(3)
            .map(|f| format!("  • {} ({})", f.file_name, format_size(f.size)))
            .collect();

        let mut lines = vec![
            Line::from(vec![
                Span::raw("From: "),
                Span::styled(&sender.alias, THEME.device_alias),
            ]),
            Line::raw(""),
            Line::from(format!("{} file(s), {}", file_count, size_str)),
            Line::raw(""),
        ];

        for file_line in file_list {
            lines.push(Line::raw(file_line));
        }

        if file_count > 3 {
            lines.push(Line::raw(format!("  ... and {} more", file_count - 3)));
        }

        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(" Y ", THEME.key),
            Span::styled(" Accept  ", THEME.key_desc),
            Span::styled(" N ", THEME.key),
            Span::styled(" Decline ", THEME.key_desc),
        ]));

        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: true });
        frame.render_widget(paragraph, inner);
    }

    fn render_progress(
        &self,
        frame: &mut ratatui::Frame,
        area: Rect,
        file_name: &str,
        received: u64,
        total: u64,
    ) {
        let block = Block::default()
            .title(" 📦 Receiving... ")
            .title_style(THEME.popup_title)
            .borders(Borders::ALL)
            .border_style(THEME.popup_border);

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let percent = if total > 0 {
            (received as f64 / total as f64 * 100.0) as u16
        } else {
            0
        };

        let lines = vec![
            Line::raw(file_name),
            Line::raw(""),
            Line::raw(format!(
                "{} / {} ({percent}%)",
                format_size(received),
                format_size(total)
            )),
        ];

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    fn render_message(
        &self,
        frame: &mut ratatui::Frame,
        area: Rect,
        text: &str,
        level: MessageLevel,
    ) {
        let (title, style) = match level {
            MessageLevel::Success => (" ✓ Success ", THEME.status_success),
            MessageLevel::Error => (" ✗ Error ", THEME.status_error),
            MessageLevel::Info => (" ℹ Info ", THEME.status_info),
        };

        let block = Block::default()
            .title(title)
            .title_style(style)
            .borders(Borders::ALL)
            .border_style(style);

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines = vec![
            Line::raw(text),
            Line::raw(""),
            Line::from(vec![
                Span::styled(" Enter ", THEME.key),
                Span::styled(" OK ", THEME.key_desc),
            ]),
        ];

        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: true });
        frame.render_widget(paragraph, inner);
    }
}

/// Calculate a centered rectangle with percentage-based sizing.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(popup_layout[1])[1]
}

/// Format bytes to human-readable size.
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

//! Receive screen showing received files.

use crate::tui::theme::THEME;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table, Widget},
};
use std::sync::{Arc, RwLock};

/// Information about a received file.
#[derive(Clone, Debug)]
pub struct ReceivedFile {
    pub file_name: String,
    pub size: u64,
    pub sender: String,
    pub time: String,
}

/// Receive screen state.
pub struct ReceiveScreen {
    pub received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    pub is_listening: bool,
    pub port: u16,
}

impl ReceiveScreen {
    pub fn new(received_files: Arc<RwLock<Vec<ReceivedFile>>>, port: u16) -> Self {
        Self {
            received_files,
            is_listening: true, // Always on
            port,
        }
    }
}

impl Widget for &ReceiveScreen {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .title(" 📥 Received Files ")
            .title_style(THEME.title)
            .borders(Borders::ALL);

        let inner = block.inner(area);
        block.render(area, buf);

        let layout = Layout::vertical([
            Constraint::Length(2), // Status
            Constraint::Min(0),    // File list
            Constraint::Length(2), // Help
        ])
        .split(inner);

        // Status line
        let status = Line::from(vec![
            Span::raw("Status: "),
            Span::styled("🟢 Listening", THEME.status_success),
            Span::raw(format!(" on port {}", self.port)),
        ]);
        Paragraph::new(status).render(layout[0], buf);

        // File list
        let files = self.received_files.read().unwrap();
        if files.is_empty() {
            let msg = Paragraph::new("No files received yet.")
                .style(THEME.normal)
                .centered();
            msg.render(layout[1], buf);
        } else {
            let rows: Vec<Row> = files
                .iter()
                .rev() // Most recent first
                .take(20)
                .map(|f| {
                    Row::new(vec![
                        f.file_name.clone(),
                        format_size(f.size),
                        f.sender.clone(),
                        f.time.clone(),
                    ])
                })
                .collect();

            let widths = [
                Constraint::Percentage(40),
                Constraint::Percentage(15),
                Constraint::Percentage(25),
                Constraint::Percentage(20),
            ];

            let table = Table::new(rows, widths).header(
                Row::new(vec!["File", "Size", "From", "Time"])
                    .style(THEME.title)
                    .bottom_margin(1),
            );
            table.render(layout[1], buf);
        }

        // Help
        let help = Line::from(vec![
            Span::styled(" Esc ", THEME.key),
            Span::styled(" Back ", THEME.key_desc),
        ]);
        Paragraph::new(help).centered().render(layout[2], buf);
    }
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

//! Receive screen showing received files.

use crate::tui::theme::THEME;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table, Widget},
};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::protocol::ReceivedFile;

/// Receive screen state.
pub struct ReceiveScreen {
    pub received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    pub is_listening: bool,
    pub port: u16,
    pub save_dir: PathBuf,
}

impl ReceiveScreen {
    pub fn new(
        received_files: Arc<RwLock<Vec<ReceivedFile>>>,
        port: u16,
        save_dir: PathBuf,
    ) -> Self {
        Self {
            received_files,
            is_listening: true, // Always on
            port,
            save_dir,
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
            Constraint::Length(3), // Status (2 lines + margin)
            Constraint::Min(0),    // File list
            Constraint::Length(2), // Help
        ])
        .split(inner);

        // Status line + where files are saved.
        let status = vec![
            Line::from(vec![
                Span::raw("Status: "),
                Span::styled("🟢 Listening", THEME.status_success),
                Span::raw(format!(" on port {}", self.port)),
            ]),
            Line::from(vec![
                Span::raw("Saving to: "),
                Span::styled(self.save_dir.display().to_string(), THEME.status_info),
            ]),
        ];
        Paragraph::new(status).render(layout[0], buf);

        // File list
        let files = self
            .received_files
            .try_read()
            .unwrap_or_else(|_| panic!("Lock poisoned"));
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
                    // The saved name may differ from the offered one after a
                    // collision rename; show what actually landed on disk.
                    let saved_name = f
                        .path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| f.file_name.clone());
                    Row::new(vec![
                        saved_name,
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

        // Help — Tab/number keys switch tabs, q quits (handled globally).
        let help = Line::from(vec![
            Span::styled(" Tab ", THEME.key),
            Span::styled(" switch tabs ", THEME.key_desc),
            Span::styled(" q ", THEME.key),
            Span::styled(" quit ", THEME.key_desc),
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

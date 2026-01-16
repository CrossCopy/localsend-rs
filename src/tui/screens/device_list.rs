//! Device list screen showing discovered LocalSend devices.

use crate::protocol::DeviceInfo;
use crate::tui::theme::THEME;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    prelude::Widget,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table, TableState},
};
use std::sync::{Arc, RwLock};

/// Device list screen state.
#[allow(dead_code)]
pub struct DeviceListScreen {
    pub devices: Arc<RwLock<Vec<DeviceInfo>>>,
    pub table_state: TableState,
    pub needs_refresh: bool,
}

#[allow(dead_code)]
impl DeviceListScreen {
    pub fn new(devices: Arc<RwLock<Vec<DeviceInfo>>>) -> Self {
        Self {
            devices,
            table_state: TableState::default(),
            needs_refresh: false,
        }
    }

    pub fn request_refresh(&mut self) {
        self.needs_refresh = true;
    }

    pub fn consume_refresh(&mut self) -> bool {
        let result = self.needs_refresh;
        self.needs_refresh = false;
        result
    }

    pub fn next(&mut self) {
        let devices = self.devices.read().unwrap();
        if devices.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => (i + 1) % devices.len(),
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn previous(&mut self) {
        let devices = self.devices.read().unwrap();
        if devices.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => {
                if i == 0 {
                    devices.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    pub fn selected_device(&self) -> Option<DeviceInfo> {
        let devices = self.devices.read().unwrap();
        self.table_state
            .selected()
            .and_then(|i| devices.get(i).cloned())
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let devices = self.devices.read().unwrap();

        let block = Block::default()
            .title(" 📱 Nearby Devices ")
            .title_style(THEME.title)
            .borders(Borders::ALL);

        let inner = block.inner(area);
        block.render(area, buf);

        let layout = Layout::vertical([
            Constraint::Min(0),    // Device table
            Constraint::Length(2), // Help text
        ])
        .split(inner);

        if devices.is_empty() {
            let msg = Paragraph::new("Scanning for devices...")
                .style(THEME.status_info)
                .centered();
            msg.render(layout[0], buf);
        } else {
            // Ensure selection
            if self.table_state.selected().is_none() && !devices.is_empty() {
                self.table_state.select(Some(0));
            }

            let rows: Vec<Row> = devices
                .iter()
                .map(|d| {
                    Row::new(vec![
                        d.alias.clone(),
                        d.ip.clone().unwrap_or_else(|| "Unknown".into()),
                        d.port.to_string(),
                        d.device_model.clone().unwrap_or_default(),
                    ])
                })
                .collect();

            let widths = [
                Constraint::Percentage(30),
                Constraint::Percentage(25),
                Constraint::Percentage(15),
                Constraint::Percentage(30),
            ];

            let table = Table::new(rows, widths)
                .header(
                    Row::new(vec!["Alias", "IP", "Port", "Model"])
                        .style(THEME.title)
                        .bottom_margin(1),
                )
                .row_highlight_style(THEME.selected)
                .highlight_symbol("▶ ");

            // We need to use StatefulWidget render, so do it manually
            ratatui::widgets::StatefulWidget::render(table, layout[0], buf, &mut self.table_state);
        }

        // Help text
        let help = Line::from(vec![
            Span::styled(" ↑/k ", THEME.key),
            Span::styled(" Up ", THEME.key_desc),
            Span::styled(" ↓/j ", THEME.key),
            Span::styled(" Down ", THEME.key_desc),
            Span::styled(" Enter ", THEME.key),
            Span::styled(" Select ", THEME.key_desc),
            Span::styled(" R ", THEME.key),
            Span::styled(" Refresh ", THEME.key_desc),
        ]);
        Paragraph::new(help).centered().render(layout[1], buf);
    }
}

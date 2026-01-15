//! Send file screen.

use crate::protocol::DeviceInfo;
use crate::tui::theme::THEME;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph, Widget},
};
use tui_input::Input;

/// Send file screen state.
pub struct SendFileScreen {
    pub target: Option<DeviceInfo>,
    pub input: Input,
    pub is_sending: bool,
    pub progress: f64,
    pub current_file: Option<String>,
}

impl Default for SendFileScreen {
    fn default() -> Self {
        Self {
            target: None,
            input: Input::default(),
            is_sending: false,
            progress: 0.0,
            current_file: None,
        }
    }
}

impl SendFileScreen {
    pub fn set_target(&mut self, device: Option<DeviceInfo>) {
        self.target = device;
    }

    pub fn clear(&mut self) {
        self.input.reset();
        self.is_sending = false;
        self.progress = 0.0;
        self.current_file = None;
    }

    pub fn file_path(&self) -> &str {
        self.input.value()
    }

    pub fn set_progress(&mut self, file: &str, progress: f64) {
        self.current_file = Some(file.to_string());
        self.progress = progress;
    }
}

impl Widget for &SendFileScreen {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .title(" 📁 Send File ")
            .title_style(THEME.title)
            .borders(Borders::ALL);

        let inner = block.inner(area);
        block.render(area, buf);

        let layout = Layout::vertical([
            Constraint::Length(3), // Target info
            Constraint::Length(3), // Input/Progress
            Constraint::Min(0),    // Spacer
            Constraint::Length(2), // Help
        ])
        .split(inner);

        // Target info
        let target_text = if let Some(ref device) = self.target {
            Line::from(vec![
                Span::raw("Target: "),
                Span::styled(&device.alias, THEME.device_alias),
                Span::raw(" ("),
                Span::styled(device.ip.as_deref().unwrap_or("Unknown"), THEME.device_ip),
                Span::raw(")"),
            ])
        } else {
            Line::styled(
                "No device selected! Go to Devices first.",
                THEME.status_error,
            )
        };
        Paragraph::new(target_text).render(layout[0], buf);

        // Input or progress
        if self.is_sending {
            let label = format!(
                "{}: {:.0}%",
                self.current_file.as_deref().unwrap_or("Uploading"),
                self.progress * 100.0
            );
            let gauge = Gauge::default()
                .block(Block::default().borders(Borders::ALL))
                .gauge_style(THEME.status_success)
                .ratio(self.progress)
                .label(label);
            gauge.render(layout[1], buf);
        } else {
            let input_block = Block::default().title(" File Path ").borders(Borders::ALL);
            let input_inner = input_block.inner(layout[1]);
            input_block.render(layout[1], buf);
            Paragraph::new(self.input.value()).render(input_inner, buf);
        }

        // Help text
        let help = if self.target.is_some() && !self.is_sending {
            Line::from(vec![
                Span::styled(" Enter ", THEME.key),
                Span::styled(" Send ", THEME.key_desc),
                Span::styled(" Esc ", THEME.key),
                Span::styled(" Back ", THEME.key_desc),
            ])
        } else {
            Line::from(vec![
                Span::styled(" Esc ", THEME.key),
                Span::styled(" Back ", THEME.key_desc),
            ])
        };
        Paragraph::new(help).centered().render(layout[3], buf);
    }
}

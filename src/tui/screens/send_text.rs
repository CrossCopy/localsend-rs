//! Send text screen.

use crate::protocol::DeviceInfo;
use crate::tui::theme::THEME;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use tui_input::Input;

/// Send text screen state.
pub struct SendTextScreen {
    pub target: Option<DeviceInfo>,
    pub input: Input,
    pub is_sending: bool,
}

impl Default for SendTextScreen {
    fn default() -> Self {
        Self {
            target: None,
            input: Input::default(),
            is_sending: false,
        }
    }
}

impl SendTextScreen {
    pub fn set_target(&mut self, device: Option<DeviceInfo>) {
        self.target = device;
    }

    pub fn clear(&mut self) {
        self.input.reset();
        self.is_sending = false;
    }

    pub fn message(&self) -> &str {
        self.input.value()
    }
}

impl Widget for &SendTextScreen {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .title(" 📝 Send Text Message ")
            .title_style(THEME.title)
            .borders(Borders::ALL);

        let inner = block.inner(area);
        block.render(area, buf);

        let layout = Layout::vertical([
            Constraint::Length(3), // Target info
            Constraint::Length(3), // Input
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

        // Input field
        let input_block = Block::default().title(" Message ").borders(Borders::ALL);
        let input_inner = input_block.inner(layout[1]);
        input_block.render(layout[1], buf);

        let input_text = if self.is_sending {
            Line::styled("Sending...", THEME.status_info)
        } else {
            Line::raw(self.input.value())
        };
        Paragraph::new(input_text).render(input_inner, buf);

        // Help text
        let help = if self.target.is_some() {
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

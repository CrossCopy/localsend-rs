//! Main menu screen.

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};

use crate::tui::theme::THEME;

/// Main menu items.
const MENU_ITEMS: &[(&str, &str)] = &[
    ("📱", "View & Select Devices"),
    ("📝", "Send Text Message"),
    ("📁", "Send File"),
    ("📥", "Received Files"),
    ("⚙️", "Settings"),
    ("🚪", "Exit"),
];

/// Main menu screen state.
#[derive(Debug, Default, Clone, Copy)]
pub struct MainMenuScreen {
    pub selected_index: usize,
}

impl MainMenuScreen {
    pub fn next(&mut self) {
        self.selected_index = (self.selected_index + 1) % MENU_ITEMS.len();
    }

    pub fn previous(&mut self) {
        self.selected_index = if self.selected_index == 0 {
            MENU_ITEMS.len() - 1
        } else {
            self.selected_index - 1
        };
    }

    pub fn items_count(&self) -> usize {
        MENU_ITEMS.len()
    }
}

impl Widget for &MainMenuScreen {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .title(" 🌐 LocalSend TUI ")
            .title_style(THEME.title)
            .borders(Borders::ALL);

        let inner = block.inner(area);
        block.render(area, buf);

        let layout = Layout::vertical([
            Constraint::Length(2), // Title spacing
            Constraint::Min(0),    // Menu items
            Constraint::Length(2), // Help text
        ])
        .split(inner);

        // Render menu items
        let mut lines = Vec::new();
        for (i, (icon, label)) in MENU_ITEMS.iter().enumerate() {
            let style = if i == self.selected_index {
                THEME.selected
            } else {
                THEME.normal
            };

            let prefix = if i == self.selected_index {
                "▶ "
            } else {
                "  "
            };
            lines.push(Line::styled(format!("{}{} {}", prefix, icon, label), style));
            lines.push(Line::raw("")); // Spacing
        }

        let menu = Paragraph::new(lines);
        menu.render(layout[1], buf);

        // Help text
        let help = Line::from(vec![
            Span::styled(" ↑/k ", THEME.key),
            Span::styled(" Up ", THEME.key_desc),
            Span::styled(" ↓/j ", THEME.key),
            Span::styled(" Down ", THEME.key_desc),
            Span::styled(" Enter ", THEME.key),
            Span::styled(" Select ", THEME.key_desc),
            Span::styled(" q ", THEME.key),
            Span::styled(" Quit ", THEME.key_desc),
        ]);
        Paragraph::new(help).centered().render(layout[2], buf);
    }
}

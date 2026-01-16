//! Main TUI application with async event loop.

use crate::client::LocalSendClient;
use crate::crypto::generate_fingerprint;
use crate::discovery::{Discovery, MulticastDiscovery};
use crate::protocol::{DeviceInfo, DeviceType, PROTOCOL_VERSION, ReceivedFile};
use crate::server::LocalSendServer;
use crate::server::PendingTransfer;

use super::popup::{MessageLevel, Popup};
use super::screens::{
    Screen, receive::ReceiveScreen, send_file::SendFileScreen, send_text::SendTextScreen,
    settings::SettingsScreen,
};
use super::theme::THEME;

use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Rect},
    symbols,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Tabs, Widget},
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use strum::IntoEnumIterator;
use tokio::time::Duration;
use tui_input::backend::crossterm::EventHandler;

/// Main TUI application state.
pub struct App {
    // Mode
    should_quit: bool,
    screen: Screen,

    // Device info
    device_info: DeviceInfo,
    port: u16,
    https: bool,
    save_dir: PathBuf,

    // Shared state
    devices: Arc<RwLock<Vec<DeviceInfo>>>,
    received_files: Arc<RwLock<Vec<ReceivedFile>>>,
    pending_transfer: Arc<RwLock<Option<PendingTransfer>>>,

    // Popup overlay
    popup: Option<Popup>,

    // Screen states
    send_text: SendTextScreen,
    send_file: SendFileScreen,
    receive: ReceiveScreen,
    settings: SettingsScreen,

    // Status message
    status_message: Option<(String, MessageLevel)>,

    // Background services
    discovery: Option<MulticastDiscovery>,
    server: Option<LocalSendServer>,
}

impl App {
    /// Create a new App instance.
    pub fn new(port: u16, alias: Option<String>, https: bool) -> Result<Self> {
        let device_name = alias.unwrap_or_else(|| {
            format!("LocalSend-Rust-{}", &uuid::Uuid::new_v4().to_string()[..4])
        });

        let device_info = DeviceInfo {
            alias: device_name,
            version: PROTOCOL_VERSION.to_string(),
            device_model: Some(crate::device::get_device_model()),
            device_type: Some(DeviceType::Desktop),
            fingerprint: generate_fingerprint(),
            port,
            protocol: if https { "https" } else { "http" }.to_string(),
            download: false,
            ip: None,
        };

        let save_dir = PathBuf::from("./downloads");
        let devices = Arc::new(RwLock::new(Vec::new()));
        let received_files = Arc::new(RwLock::new(Vec::new()));
        let pending_transfer = Arc::new(RwLock::new(None));

        Ok(Self {
            should_quit: false,
            screen: Screen::SendText,
            device_info: device_info.clone(),
            port,
            https,
            save_dir: save_dir.clone(),
            devices: devices.clone(),
            received_files: received_files.clone(),
            pending_transfer,
            popup: None,

            send_text: SendTextScreen::new(devices.clone()),
            send_file: SendFileScreen::new(devices.clone()),
            receive: ReceiveScreen::new(received_files.clone(), port),
            settings: SettingsScreen::new(device_info, save_dir.to_string_lossy().into_owned()),
            status_message: None,
            discovery: None,
            server: None,
        })
    }

    /// Run the TUI application.
    pub async fn run(mut self, mut terminal: DefaultTerminal) -> Result<()> {
        // Start background services
        self.start_discovery().await?;
        self.start_server().await?;

        // Main event loop
        let tick_rate = Duration::from_millis(100);

        while !self.should_quit {
            // Render
            terminal.draw(|frame| self.render(frame))?;

            // Check for pending transfers (popup trigger)
            self.check_pending_transfer();

            // Handle events with timeout
            if event::poll(tick_rate)?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.handle_key(key.code);
            }
        }

        Ok(())
    }

    /// Start multicast discovery in background.
    async fn start_discovery(&mut self) -> Result<()> {
        let devices = self.devices.clone();
        let device_info = self.device_info.clone();

        let mut discovery = MulticastDiscovery::new_with_device(device_info.clone());

        discovery.on_discovered(move |device: DeviceInfo| {
            // Skip self
            if device.fingerprint == device_info.fingerprint {
                return;
            }

            let mut devices_guard = devices.write().unwrap();
            let exists = devices_guard.iter().any(|d| {
                d.fingerprint == device.fingerprint || (d.ip == device.ip && d.port == device.port)
            });
            if !exists {
                devices_guard.push(device);
            }
        });

        discovery.start().await?;
        discovery.announce_presence().await?;

        self.discovery = Some(discovery);

        Ok(())
    }

    /// Start receiver server in background.
    async fn start_server(&mut self) -> Result<()> {
        // Ensure save directory exists
        if !self.save_dir.exists() {
            std::fs::create_dir_all(&self.save_dir)?;
        }

        let mut server = LocalSendServer::new_with_device(
            self.device_info.clone(),
            self.save_dir.clone(),
            self.https,
            self.pending_transfer.clone(),
            self.received_files.clone(),
        )?;

        #[cfg(feature = "https")]
        if self.https {
            let cert = crate::crypto::generate_tls_certificate()?;
            server.set_tls_certificate(cert);
        }

        server.start(None).await?;

        self.server = Some(server);

        Ok(())
    }

    /// Check for pending transfer and show popup.
    fn check_pending_transfer(&mut self) {
        if self.popup.is_some() {
            return; // Already showing a popup
        }

        let mut pending = self.pending_transfer.write().unwrap();
        if let Some(transfer) = pending.take() {
            self.popup = Some(Popup::TransferConfirm {
                sender: transfer.sender,
                files: transfer.files,
                response_tx: transfer.response_tx,
            });
        }
    }

    /// Handle key press.
    fn handle_key(&mut self, key: KeyCode) {
        // Popup takes priority
        if self.popup.is_some() {
            self.handle_popup_key(key);
            return;
        }

        // Global keys
        match key {
            KeyCode::Char('q') => {
                self.should_quit = true;
                return;
            }
            KeyCode::Esc => {
                let handles_esc = match self.screen {
                    Screen::SendText => {
                        self.send_text.stage
                            == crate::tui::screens::send_text::SendTextStage::EnterMessage
                    }
                    Screen::SendFile => {
                        self.send_file.stage
                            == crate::tui::screens::send_file::SendFileStage::EnterFilePath
                    }
                    _ => false,
                };

                if !handles_esc {
                    self.status_message = Some(("Press q to quit".into(), MessageLevel::Info));
                }
            }
            KeyCode::Right | KeyCode::Tab => {
                // Only allow switching tabs if not in input mode
                let can_switch = match self.screen {
                    Screen::SendText => {
                        self.send_text.stage
                            == crate::tui::screens::send_text::SendTextStage::SelectDevice
                    }
                    Screen::SendFile => {
                        self.send_file.stage
                            == crate::tui::screens::send_file::SendFileStage::SelectDevice
                    }
                    _ => true,
                };

                if can_switch {
                    let screens: Vec<_> = Screen::iter().collect();
                    let current_index = screens.iter().position(|&s| s == self.screen).unwrap_or(0);
                    self.screen = screens[(current_index + 1) % screens.len()];
                    return;
                }
            }
            KeyCode::Left => {
                let can_switch = match self.screen {
                    Screen::SendText => {
                        self.send_text.stage
                            == crate::tui::screens::send_text::SendTextStage::SelectDevice
                    }
                    Screen::SendFile => {
                        self.send_file.stage
                            == crate::tui::screens::send_file::SendFileStage::SelectDevice
                    }
                    _ => true,
                };

                if can_switch {
                    let screens: Vec<_> = Screen::iter().collect();
                    let current_index = screens.iter().position(|&s| s == self.screen).unwrap_or(0);
                    self.screen = screens[(current_index + screens.len() - 1) % screens.len()];
                    return;
                }
            }
            _ => {}
        }

        match self.screen {
            Screen::SendText => self.handle_send_text_key(key),
            Screen::SendFile => self.handle_send_file_key(key),
            Screen::Receive => self.handle_receive_key(key),
            Screen::Settings => self.handle_settings_key(key),
        }

        // Check for refresh requests
        let mut refresh = self.send_text.consume_refresh();
        refresh |= self.send_file.consume_refresh();

        if refresh {
            self.devices.write().unwrap().clear();
            if let Some(ref discovery) = self.discovery {
                let discovery = discovery.clone();
                tokio::spawn(async move {
                    let _ = discovery.announce_presence().await;
                });
            }
            self.status_message = Some(("Refreshing devices...".into(), MessageLevel::Info));
        }
    }

    fn handle_popup_key(&mut self, key: KeyCode) {
        match &mut self.popup {
            Some(Popup::TransferConfirm { .. }) => {
                match key {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                        // Accept - we need to take ownership of the sender
                        if let Some(Popup::TransferConfirm { response_tx, .. }) = self.popup.take()
                        {
                            let _ = response_tx.send(true);
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        // Decline
                        if let Some(Popup::TransferConfirm { response_tx, .. }) = self.popup.take()
                        {
                            let _ = response_tx.send(false);
                        }
                    }
                    _ => {}
                }
            }
            Some(Popup::Message { .. }) => {
                if matches!(key, KeyCode::Enter | KeyCode::Esc) {
                    self.popup = None;
                }
            }
            Some(Popup::TransferProgress { .. }) => {
                // Progress popup is non-interactive
            }
            None => {}
        }
    }

    fn handle_send_text_key(&mut self, key: KeyCode) {
        use crate::tui::screens::send_text::SendTextStage;

        match self.send_text.stage {
            SendTextStage::SelectDevice => match key {
                KeyCode::Up | KeyCode::Char('k') => self.send_text.previous_device(),
                KeyCode::Down | KeyCode::Char('j') => self.send_text.next_device(),
                KeyCode::Enter => self.send_text.select_current_device(),
                KeyCode::Char('r') | KeyCode::Char('R') => self.send_text.request_refresh(),
                _ => {}
            },
            SendTextStage::EnterMessage => match key {
                KeyCode::Esc => self.send_text.stage = SendTextStage::SelectDevice,
                KeyCode::Enter => {
                    if let Some(target) = &self.send_text.selected_device
                        && !self.send_text.message().is_empty()
                    {
                        let message = self.send_text.message().to_string();
                        let target = target.clone();
                        let device_info = self.device_info.clone();

                        self.send_text.is_sending = true;

                        tokio::spawn(async move {
                            let client = LocalSendClient::new(device_info);
                            let _ = send_text_message(&client, &target, &message).await;
                        });

                        self.send_text.clear();
                        self.status_message =
                            Some(("Sending message...".into(), MessageLevel::Info));
                    }
                }
                _ => {
                    self.send_text
                        .input
                        .handle_event(&Event::Key(event::KeyEvent::new(
                            key,
                            event::KeyModifiers::NONE,
                        )));
                }
            },
        }
    }

    fn handle_send_file_key(&mut self, key: KeyCode) {
        use crate::tui::screens::send_file::SendFileStage;

        match self.send_file.stage {
            SendFileStage::SelectDevice => match key {
                KeyCode::Up | KeyCode::Char('k') => self.send_file.previous_device(),
                KeyCode::Down | KeyCode::Char('j') => self.send_file.next_device(),
                KeyCode::Enter => self.send_file.select_current_device(),
                KeyCode::Char('r') | KeyCode::Char('R') => self.send_file.request_refresh(),
                _ => {}
            },
            SendFileStage::EnterFilePath => match key {
                KeyCode::Esc => self.send_file.stage = SendFileStage::SelectDevice,
                KeyCode::Enter => {
                    if let Some(target) = &self.send_file.selected_device
                        && !self.send_file.file_path().is_empty()
                    {
                        let file_path = PathBuf::from(self.send_file.file_path());
                        if file_path.exists() {
                            let target = target.clone();
                            let device_info = self.device_info.clone();

                            self.send_file.is_sending = true;

                            tokio::spawn(async move {
                                let client = LocalSendClient::new(device_info);
                                let _ = send_file(&client, &target, &file_path).await;
                            });

                            self.send_file.clear();
                            self.status_message =
                                Some(("Sending file...".into(), MessageLevel::Info));
                        } else {
                            self.status_message =
                                Some(("File not found".into(), MessageLevel::Error));
                        }
                    }
                }
                _ => {
                    self.send_file
                        .input
                        .handle_event(&Event::Key(event::KeyEvent::new(
                            key,
                            event::KeyModifiers::NONE,
                        )));
                }
            },
        }
    }

    fn handle_receive_key(&mut self, _key: KeyCode) {}

    fn handle_settings_key(&mut self, _key: KeyCode) {}

    /// Render the TUI.
    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();

        // Main layout: header, content, status bar
        let layout = Layout::vertical([
            Constraint::Length(3), // Header/Tabs
            Constraint::Min(0),    // Content
            Constraint::Length(1), // Status bar
        ])
        .split(area);

        // Header with Tabs
        self.render_header(frame, layout[0]);

        // Content based on screen
        match self.screen {
            Screen::SendText => self.send_text.render(layout[1], frame.buffer_mut()),
            Screen::SendFile => self.send_file.render(layout[1], frame.buffer_mut()),
            Screen::Receive => frame.render_widget(&self.receive, layout[1]),
            Screen::Settings => frame.render_widget(&self.settings, layout[1]),
        }

        // Status bar
        self.render_status_bar(frame, layout[2]);

        // Popup overlay (if any)
        if let Some(ref popup) = self.popup {
            popup.render(frame);
        }
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default().style(THEME.root).borders(Borders::BOTTOM);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let layout = Layout::horizontal([
            Constraint::Length(15), // Title
            Constraint::Min(0),     // Tabs
        ])
        .split(inner);

        // Title
        let title = Line::from(vec![Span::styled(" 🌐 LocalSend ", THEME.title)]);
        frame.render_widget(Paragraph::new(title), layout[0]);

        // Tabs
        let titles: Vec<String> = Screen::iter()
            .map(|s| match s {
                Screen::SendText => "📝 Text".to_string(),
                Screen::SendFile => "📁 File".to_string(),
                Screen::Receive => "📥 Inbox".to_string(),
                Screen::Settings => "⚙️ Settings".to_string(),
            })
            .collect();

        let current_index = Screen::iter().position(|s| s == self.screen).unwrap_or(0);
        let tabs = Tabs::new(titles)
            .block(Block::default())
            .select(current_index)
            .style(THEME.normal)
            .highlight_style(THEME.selected)
            .divider(symbols::DOT);

        frame.render_widget(tabs, layout[1]);
    }

    fn render_status_bar(&mut self, frame: &mut Frame, area: Rect) {
        let devices_count = self.devices.read().unwrap().len();

        let mut spans = vec![
            Span::styled(format!("📲 {}", self.device_info.alias), THEME.device_alias),
            Span::raw(" | "),
            Span::styled(format!("📱 {} devices ", devices_count), THEME.status_bar),
            Span::raw("| "),
            Span::styled(format!("🟢 Listening on {} ", self.port), THEME.status_bar),
        ];

        if let Some((ref msg, level)) = self.status_message {
            spans.push(Span::raw("| "));
            let style = match level {
                MessageLevel::Success => THEME.status_success,
                MessageLevel::Error => THEME.status_error,
                MessageLevel::Info => THEME.status_info,
            };
            spans.push(Span::styled(msg.clone(), style));
        }

        let line = Line::from(spans);
        frame.render_widget(Paragraph::new(line).style(THEME.status_bar), area);
    }
}

async fn send_text_message(
    client: &LocalSendClient,
    target: &DeviceInfo,
    message: &str,
) -> anyhow::Result<()> {
    use crate::file::{build_file_metadata_from_bytes, generate_file_id};

    let file_data = message.as_bytes().to_vec();
    let file_name = "message.txt".to_string();
    let file_id = generate_file_id();

    let mut metadata = build_file_metadata_from_bytes(
        file_id,
        file_name,
        "text/plain".to_string(),
        file_data.clone(),
    );
    metadata.preview = Some(message.to_string());

    let mut files = HashMap::new();
    files.insert(metadata.id.clone(), metadata.clone());

    let response = client.prepare_upload(target, files, None).await?;

    if response.session_id.is_empty() {
        // 204 No Content - text message sent via preview
        return Ok(());
    }

    // Write to temp file and upload
    if let Some(token) = response.files.get(&metadata.id) {
        let temp_path = std::env::temp_dir().join(format!("localsend_text_{}.txt", metadata.id));
        tokio::fs::write(&temp_path, &file_data).await?;

        client
            .upload_file(
                target,
                &response.session_id,
                &metadata.id,
                token,
                &temp_path,
                None,
            )
            .await?;

        let _ = tokio::fs::remove_file(temp_path).await;
    }

    Ok(())
}

/// Send a file to a device.
async fn send_file(
    client: &LocalSendClient,
    target: &DeviceInfo,
    file_path: &PathBuf,
) -> anyhow::Result<()> {
    use crate::file::build_file_metadata;

    let metadata = build_file_metadata(file_path).await?;

    let mut files = HashMap::new();
    files.insert(metadata.id.clone(), metadata.clone());

    let response = client.prepare_upload(target, files, None).await?;

    if let Some(token) = response.files.get(&metadata.id) {
        client
            .upload_file(
                target,
                &response.session_id,
                &metadata.id,
                token,
                file_path,
                None,
            )
            .await?;
    }

    Ok(())
}

/// Main entry point for the TUI.
pub async fn run_tui(port: u16, alias: Option<String>, https: bool) -> Result<()> {
    color_eyre::install()?;

    let terminal = ratatui::init();
    let app_result = App::new(port, alias, https)?.run(terminal).await;
    ratatui::restore();

    app_result
}

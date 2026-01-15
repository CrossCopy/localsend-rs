//! Main TUI application with async event loop.

use crate::client::LocalSendClient;
use crate::crypto::generate_fingerprint;
use crate::discovery::{Discovery, MulticastDiscovery};
use crate::protocol::{DeviceInfo, DeviceType, FileMetadata, PROTOCOL_VERSION};
use crate::server::LocalSendServer;

use super::popup::{MessageLevel, Popup};
use super::screens::receive::ReceivedFile;
use super::screens::{
    Screen, device_list::DeviceListScreen, main_menu::MainMenuScreen, receive::ReceiveScreen,
    send_file::SendFileScreen, send_text::SendTextScreen, settings::SettingsScreen,
};
use super::theme::THEME;

use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tui_input::backend::crossterm::EventHandler;

/// Pending incoming transfer request.
pub struct PendingTransfer {
    pub sender: DeviceInfo,
    pub files: HashMap<String, FileMetadata>,
    pub response_tx: oneshot::Sender<bool>,
}

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
    main_menu: MainMenuScreen,
    device_list: DeviceListScreen,
    send_text: SendTextScreen,
    send_file: SendFileScreen,
    receive: ReceiveScreen,
    settings: SettingsScreen,

    // Selected device for sending
    selected_device: Option<DeviceInfo>,

    // Status message
    status_message: Option<(String, MessageLevel)>,

    // Background task handles
    _discovery_handle: Option<JoinHandle<()>>,
    _server_handle: Option<JoinHandle<()>>,
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
            screen: Screen::MainMenu,
            device_info: device_info.clone(),
            port,
            https,
            save_dir: save_dir.clone(),
            devices: devices.clone(),
            received_files: received_files.clone(),
            pending_transfer,
            popup: None,
            main_menu: MainMenuScreen::default(),
            device_list: DeviceListScreen::new(devices.clone()),
            send_text: SendTextScreen::default(),
            send_file: SendFileScreen::default(),
            receive: ReceiveScreen::new(received_files.clone(), port),
            settings: SettingsScreen::new(device_info, save_dir.to_string_lossy().into_owned()),
            selected_device: None,
            status_message: None,
            _discovery_handle: None,
            _server_handle: None,
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
            if event::poll(tick_rate)? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        self.handle_key(key.code);
                    }
                }
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
        )?;

        #[cfg(feature = "https")]
        if self.https {
            let cert = crate::crypto::generate_tls_certificate()?;
            server.set_tls_certificate(cert);
        }

        server.start(None).await?;

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

        match self.screen {
            Screen::MainMenu => self.handle_main_menu_key(key),
            Screen::DeviceList => self.handle_device_list_key(key),
            Screen::SendText => self.handle_send_text_key(key),
            Screen::SendFile => self.handle_send_file_key(key),
            Screen::Receive => self.handle_receive_key(key),
            Screen::Settings => self.handle_settings_key(key),
        }
    }

    fn handle_popup_key(&mut self, key: KeyCode) {
        match &mut self.popup {
            Some(Popup::TransferConfirm { response_tx, .. }) => {
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

    fn handle_main_menu_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.main_menu.previous(),
            KeyCode::Down | KeyCode::Char('j') => self.main_menu.next(),
            KeyCode::Enter => match self.main_menu.selected_index {
                0 => self.screen = Screen::DeviceList,
                1 => {
                    self.send_text.set_target(self.selected_device.clone());
                    self.screen = Screen::SendText;
                }
                2 => {
                    self.send_file.set_target(self.selected_device.clone());
                    self.screen = Screen::SendFile;
                }
                3 => self.screen = Screen::Receive,
                4 => self.screen = Screen::Settings,
                5 => self.should_quit = true,
                _ => {}
            },
            _ => {}
        }
    }

    fn handle_device_list_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => self.screen = Screen::MainMenu,
            KeyCode::Up | KeyCode::Char('k') => self.device_list.previous(),
            KeyCode::Down | KeyCode::Char('j') => self.device_list.next(),
            KeyCode::Enter => {
                if let Some(device) = self.device_list.selected_device() {
                    self.selected_device = Some(device);
                    self.status_message = Some((
                        format!("Selected: {}", self.selected_device.as_ref().unwrap().alias),
                        MessageLevel::Success,
                    ));
                    self.screen = Screen::MainMenu;
                }
            }
            _ => {}
        }
    }

    fn handle_send_text_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.send_text.clear();
                self.screen = Screen::MainMenu;
            }
            KeyCode::Enter => {
                if self.selected_device.is_some() && !self.send_text.message().is_empty() {
                    let message = self.send_text.message().to_string();
                    let target = self.selected_device.clone().unwrap();
                    let device_info = self.device_info.clone();

                    self.send_text.is_sending = true;

                    // Spawn send task
                    tokio::spawn(async move {
                        let client = LocalSendClient::new(device_info);
                        let _ = send_text_message(&client, &target, &message).await;
                    });

                    self.send_text.clear();
                    self.status_message = Some(("Sending message...".into(), MessageLevel::Info));
                    self.screen = Screen::MainMenu;
                }
            }
            _ => {
                // Forward to input handler
                self.send_text
                    .input
                    .handle_event(&Event::Key(event::KeyEvent::new(
                        key,
                        event::KeyModifiers::NONE,
                    )));
            }
        }
    }

    fn handle_send_file_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.send_file.clear();
                self.screen = Screen::MainMenu;
            }
            KeyCode::Enter => {
                if self.selected_device.is_some() && !self.send_file.file_path().is_empty() {
                    let file_path = PathBuf::from(self.send_file.file_path());
                    if file_path.exists() {
                        let target = self.selected_device.clone().unwrap();
                        let device_info = self.device_info.clone();

                        self.send_file.is_sending = true;

                        tokio::spawn(async move {
                            let client = LocalSendClient::new(device_info);
                            let _ = send_file(&client, &target, &file_path).await;
                        });

                        self.send_file.clear();
                        self.status_message = Some(("Sending file...".into(), MessageLevel::Info));
                        self.screen = Screen::MainMenu;
                    } else {
                        self.status_message = Some(("File not found".into(), MessageLevel::Error));
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
        }
    }

    fn handle_receive_key(&mut self, key: KeyCode) {
        if let KeyCode::Esc = key {
            self.screen = Screen::MainMenu;
        }
    }

    fn handle_settings_key(&mut self, key: KeyCode) {
        if let KeyCode::Esc = key {
            self.screen = Screen::MainMenu;
        }
    }

    /// Render the TUI.
    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();

        // Main layout: header, content, status bar
        let layout = Layout::vertical([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // Content
            Constraint::Length(1), // Status bar
        ])
        .split(area);

        // Header
        self.render_header(frame, layout[0]);

        // Content based on screen
        match self.screen {
            Screen::MainMenu => frame.render_widget(&self.main_menu, layout[1]),
            Screen::DeviceList => self.device_list.render(layout[1], frame.buffer_mut()),
            Screen::SendText => frame.render_widget(&self.send_text, layout[1]),
            Screen::SendFile => frame.render_widget(&self.send_file, layout[1]),
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

        let layout = Layout::horizontal([Constraint::Min(0), Constraint::Length(30)]).split(inner);

        // Title
        let title = Line::from(vec![
            Span::styled(" 🌐 LocalSend TUI ", THEME.title),
            Span::raw("| "),
            Span::styled(&self.device_info.alias, THEME.device_alias),
        ]);
        frame.render_widget(Paragraph::new(title), layout[0]);

        // Selected device
        let selected = if let Some(ref device) = self.selected_device {
            Line::from(vec![
                Span::raw("Target: "),
                Span::styled(&device.alias, THEME.device_alias),
            ])
        } else {
            Line::styled("No target selected", THEME.status_info)
        };
        frame.render_widget(Paragraph::new(selected).right_aligned(), layout[1]);
    }

    fn render_status_bar(&mut self, frame: &mut Frame, area: Rect) {
        let devices_count = self.devices.read().unwrap().len();

        let mut spans = vec![
            Span::styled(format!(" 📱 {} devices ", devices_count), THEME.status_bar),
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

//! Main TUI application with async event loop.

use crate::client::LocalSendClient;
use crate::crypto::generate_fingerprint;
use crate::discovery::{Discovery, MulticastDiscovery};
use crate::protocol::{DeviceInfo, DeviceType, PROTOCOL_VERSION, Protocol, ReceivedFile};
use crate::server::{LocalSendServer, ServerEvent};

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
    widgets::{Block, Borders, Paragraph, Tabs},
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use strum::IntoEnumIterator;
use tokio::sync::RwLock;
use tokio::time::Duration;
use tui_input::backend::crossterm::EventHandler;

/// Which send flow a background task belongs to, so its result updates the right screen.
#[derive(Debug, Clone, Copy)]
enum SendKind {
    File,
    Text,
}

/// Progress and completion reported by a spawned send task back to the UI loop.
/// Send tasks run detached; without this channel a failure was silently swallowed
/// (`let _ = send_file(...).await`) and the progress gauge never moved.
#[derive(Debug)]
enum SendUpdate {
    /// Cumulative bytes sent for the in-flight file upload.
    Progress {
        generation: u64,
        sent: u64,
        total: u64,
    },
    /// The send finished. `error` is `None` on success, or the failure reason.
    Finished {
        generation: u64,
        kind: SendKind,
        label: String,
        error: Option<String>,
    },
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
    events_rx: Option<tokio::sync::mpsc::Receiver<ServerEvent>>,

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

    // Back-channel from spawned send tasks (progress + result).
    send_tx: tokio::sync::mpsc::UnboundedSender<SendUpdate>,
    send_rx: tokio::sync::mpsc::UnboundedReceiver<SendUpdate>,
    // Bumped whenever a send starts or is cancelled; updates from an older
    // generation (a cancelled/abandoned task) are ignored so they can't clobber
    // a newer send or wedge `is_sending`.
    send_generation: u64,
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
            device_model: Some(crate::core::device::get_device_model()),
            device_type: Some(DeviceType::Desktop),
            fingerprint: generate_fingerprint(),
            port,
            protocol: if https {
                Protocol::Https
            } else {
                Protocol::Http
            },
            download: false,
            ip: None,
        };

        let save_dir = PathBuf::from("./downloads");
        let devices = Arc::new(RwLock::new(Vec::new()));
        let received_files = Arc::new(RwLock::new(Vec::new()));
        let (send_tx, send_rx) = tokio::sync::mpsc::unbounded_channel();

        Ok(Self {
            should_quit: false,
            screen: Screen::SendText,
            device_info: device_info.clone(),
            port,
            https,
            save_dir: save_dir.clone(),
            devices: devices.clone(),
            received_files: received_files.clone(),
            events_rx: None,
            popup: None,

            send_text: SendTextScreen::new(devices.clone()),
            send_file: SendFileScreen::new(devices.clone()),
            receive: ReceiveScreen::new(received_files.clone(), port, save_dir.clone()),
            settings: SettingsScreen::new(device_info, save_dir.to_string_lossy().into_owned()),
            status_message: None,
            discovery: None,
            server: None,
            send_tx,
            send_rx,
            send_generation: 0,
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
            self.poll_server_events();

            // Apply progress/results from background send tasks.
            self.poll_send_updates();

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

            let mut devices_guard = devices
                .try_write()
                .unwrap_or_else(|_| panic!("Lock poisoned"));
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

        let protocol = if self.https {
            Protocol::Https
        } else {
            Protocol::Http
        };

        let (server, events) = LocalSendServer::builder()
            .alias(self.device_info.alias.clone())
            .port(self.port)
            .save_dir(self.save_dir.clone())
            .protocol(protocol)
            .auto_accept(false)
            .build()
            .await?;

        self.events_rx = Some(events);
        self.server = Some(server);

        Ok(())
    }

    /// Drain pending `ServerEvent`s and react (show popup, record received files).
    fn poll_server_events(&mut self) {
        let Some(rx) = self.events_rx.as_mut() else {
            return;
        };
        while let Ok(ev) = rx.try_recv() {
            match ev {
                ServerEvent::TransferRequest(request) => {
                    if self.popup.is_none() {
                        self.popup = Some(Popup::TransferConfirm { request });
                    } else {
                        request.decline(); // busy with another dialog
                    }
                }
                ServerEvent::FileReceived {
                    file_name,
                    path,
                    size,
                    sender_alias,
                    ..
                } => {
                    self.received_files
                        .try_write()
                        .unwrap_or_else(|_| panic!("Lock poisoned"))
                        .push(ReceivedFile {
                            file_name,
                            size,
                            sender: sender_alias,
                            time: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                            path,
                        });
                }
                ServerEvent::SessionDone { .. } => {
                    self.status_message =
                        Some(("✓ Transfer complete".to_string(), MessageLevel::Success));
                }
            }
        }
    }

    /// Drain progress/results from background send tasks and reflect them in the UI.
    /// Updates from a superseded generation (a cancelled send) are dropped.
    fn poll_send_updates(&mut self) {
        while let Ok(update) = self.send_rx.try_recv() {
            match update {
                SendUpdate::Progress {
                    generation,
                    sent,
                    total,
                } => {
                    if generation != self.send_generation {
                        continue;
                    }
                    let ratio = if total > 0 {
                        (sent as f64 / total as f64).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    self.send_file.progress = ratio;
                }
                SendUpdate::Finished {
                    generation,
                    kind,
                    label,
                    error,
                } => {
                    if generation != self.send_generation {
                        continue; // a cancelled/superseded send — ignore its result
                    }
                    match kind {
                        SendKind::File => self.send_file.clear(),
                        SendKind::Text => self.send_text.clear(),
                    }
                    self.status_message = Some(match error {
                        None => (format!("✓ Sent {label}"), MessageLevel::Success),
                        Some(reason) => (format!("✗ Send failed: {reason}"), MessageLevel::Error),
                    });
                }
            }
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
            self.devices
                .try_write()
                .unwrap_or_else(|_| panic!("Lock poisoned"))
                .clear();
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
                        // Accept - we need to take ownership of the request
                        if let Some(Popup::TransferConfirm { request }) = self.popup.take() {
                            request.accept();
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        // Decline
                        if let Some(Popup::TransferConfirm { request }) = self.popup.take() {
                            request.decline();
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
                KeyCode::Esc => {
                    // Leaving cancels any in-flight send (its result is ignored).
                    if self.send_text.is_sending {
                        self.send_generation = self.send_generation.wrapping_add(1);
                        self.send_text.is_sending = false;
                        self.status_message = Some(("Send cancelled".into(), MessageLevel::Info));
                    }
                    self.send_text.stage = SendTextStage::SelectDevice;
                }
                KeyCode::Enter => {
                    if self.send_text.is_sending {
                        return; // a send is already in flight
                    }
                    if let Some(target) = &self.send_text.selected_device
                        && !self.send_text.message().is_empty()
                    {
                        let message = self.send_text.message().to_string();
                        let target = target.clone();
                        let device_info = self.device_info.clone();
                        let tx = self.send_tx.clone();
                        self.send_generation = self.send_generation.wrapping_add(1);
                        let generation = self.send_generation;

                        self.send_text.is_sending = true;
                        self.status_message =
                            Some(("Sending message...".into(), MessageLevel::Info));

                        tokio::spawn(async move {
                            let client = LocalSendClient::new(device_info);
                            let result = send_text_message(&client, &target, &message).await;
                            let _ = tx.send(SendUpdate::Finished {
                                generation,
                                kind: SendKind::Text,
                                label: "message".to_string(),
                                error: result.err().map(|e| e.to_string()),
                            });
                        });
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
                KeyCode::Esc => {
                    // Leaving cancels any in-flight send (its result is ignored),
                    // so a stalled upload never wedges the screen.
                    if self.send_file.is_sending {
                        self.send_generation = self.send_generation.wrapping_add(1);
                        self.status_message = Some(("Send cancelled".into(), MessageLevel::Info));
                    }
                    self.send_file.clear();
                }
                KeyCode::Enter => {
                    if self.send_file.is_sending {
                        return; // a send is already in flight
                    }
                    if let Some(target) = &self.send_file.selected_device
                        && !self.send_file.file_path().is_empty()
                    {
                        let file_path = PathBuf::from(self.send_file.file_path());
                        if file_path.exists() {
                            let target = target.clone();
                            let device_info = self.device_info.clone();
                            let tx = self.send_tx.clone();
                            self.send_generation = self.send_generation.wrapping_add(1);
                            let generation = self.send_generation;
                            let label = file_path
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "file".to_string());

                            // Keep the sending screen (with the gauge) up; the Finished
                            // update clears it. Don't clear() here or the gauge never shows.
                            self.send_file.is_sending = true;
                            self.send_file.progress = 0.0;
                            self.send_file.current_file = Some(label.clone());
                            self.status_message =
                                Some((format!("Sending {label}..."), MessageLevel::Info));

                            tokio::spawn(async move {
                                let client = LocalSendClient::new(device_info);
                                let tx_prog = tx.clone();
                                let cb: crate::client::client::ProgressCallback =
                                    Box::new(move |sent, total, _elapsed| {
                                        let _ = tx_prog.send(SendUpdate::Progress {
                                            generation,
                                            sent,
                                            total,
                                        });
                                    });
                                let result =
                                    send_file(&client, &target, &file_path, Some(cb)).await;
                                let _ = tx.send(SendUpdate::Finished {
                                    generation,
                                    kind: SendKind::File,
                                    label,
                                    error: result.err().map(|e| e.to_string()),
                                });
                            });
                        } else {
                            self.status_message =
                                Some(("File not found".into(), MessageLevel::Error));
                        }
                    }
                }
                _ if !self.send_file.is_sending => {
                    self.send_file
                        .input
                        .handle_event(&Event::Key(event::KeyEvent::new(
                            key,
                            event::KeyModifiers::NONE,
                        )));
                }
                _ => {}
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
        let devices_count = self
            .devices
            .try_read()
            .unwrap_or_else(|_| panic!("Lock poisoned"))
            .len();

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
    use crate::core::file::{build_file_metadata_from_bytes, generate_file_id};

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

/// Send a file to a device, reporting per-chunk progress through `progress`.
async fn send_file(
    client: &LocalSendClient,
    target: &DeviceInfo,
    file_path: &Path,
    progress: Option<crate::client::client::ProgressCallback>,
) -> anyhow::Result<()> {
    use crate::core::file::build_file_metadata;

    let metadata = build_file_metadata(file_path).await?;

    let mut files = HashMap::new();
    files.insert(metadata.id.clone(), metadata.clone());

    let response = client.prepare_upload(target, files, None).await?;

    let token = response.files.get(&metadata.id).ok_or_else(|| {
        anyhow::anyhow!("receiver declined the file (no upload token was issued)")
    })?;

    client
        .upload_file(
            target,
            &response.session_id,
            &metadata.id,
            token,
            file_path,
            progress,
        )
        .await?;

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

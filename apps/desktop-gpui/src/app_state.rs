//! The window shell: toolbar, sidebar, one visible pane.
//!
//! All UI-only state lives here — which pane is showing, which peer is selected,
//! what files are staged, the settings form — because none of it belongs to the
//! transfer state machine. The store owns the facts the service reports; this owns
//! the choices the person makes.

use std::ffi::OsStr;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarGroup, SidebarMenu, SidebarMenuItem,
};
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Icon, Sizable as _, TitleBar};
use gpui_kit::*;

use crate::branding;
use crate::bridge::HostRuntime;
use crate::service::{
    NoticeLevel, ServerStatus, ServiceEvent, ServiceHandle, Settings, MAX_SURFACE_OPACITY,
};
use crate::store::TransferStore;
use crate::theme;
use crate::theme::s;
use crate::update::{ReleaseInfo, StagedUpdate};
use crate::views;
use crate::views::settings::SettingsForm;

/// Whether this binary was built with the pinned publisher identity required by
/// the updater. This is compile-time configuration, never a process env override.
pub(crate) const UPDATE_SIGNING_TRUST_CONFIGURED: bool =
    option_env!("LOCALSEND_GPUI_TRUSTED_TEAM_ID").is_some();

pub(crate) const fn update_signing_trust_message() -> &'static str {
    if UPDATE_SIGNING_TRUST_CONFIGURED {
        "This build has a configured build-time publisher Team ID for update verification."
    } else {
        "This build has no pinned publisher Team ID. It can check releases, but cannot download, stage, or install updates."
    }
}

fn schedule_startup_update_check(is_live: bool, schedule: impl FnOnce()) {
    if is_live {
        schedule();
    }
}

/// User-facing update progress. The staged bundle is owned here so its private
/// workspace stays alive until the user closes the window or the state changes.
pub(crate) enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(ReleaseInfo),
    ConfirmingDownload(ReleaseInfo),
    Staging(ReleaseInfo),
    Staged {
        release: ReleaseInfo,
        update: StagedUpdate,
    },
    Installing(ReleaseInfo),
    Failed(String),
}

fn staged_version(state: &UpdateState) -> Option<String> {
    match state {
        UpdateState::Available(release)
        | UpdateState::ConfirmingDownload(release)
        | UpdateState::Staging(release)
        | UpdateState::Installing(release) => Some(release.version().to_string()),
        UpdateState::Staged { update, .. } => Some(update.version().to_string()),
        UpdateState::Idle
        | UpdateState::Checking
        | UpdateState::UpToDate
        | UpdateState::Failed(_) => None,
    }
}

fn request_install_confirmation(state: &mut InstallState, has_staged_update: bool) -> bool {
    if has_staged_update && matches!(state, InstallState::Idle | InstallState::Failed { .. }) {
        *state = InstallState::Confirming;
        true
    } else {
        false
    }
}

fn cancel_install_confirmation(state: &mut InstallState) -> bool {
    if *state == InstallState::Confirming {
        *state = InstallState::Idle;
        true
    } else {
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InstallState {
    Idle,
    Confirming,
    Preparing,
    WaitingForTransfers,
    Resuming,
    StoppingListener,
    Installing,
    Failed {
        version: String,
        message: String,
        resume_error: Option<String>,
    },
    RelaunchRequested {
        version: String,
        previous_bundle: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallControl {
    Retry,
    Cancel,
    Commit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InstallProgress {
    CheckingBundle,
    Preparing,
    WaitingForTransfers,
    Resuming,
    StoppingListener,
    AwaitingInstallCommit,
    Installing,
    RelaunchRequested {
        version: String,
        previous_bundle: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InstallReceipt {
    version: String,
    previous_bundle: PathBuf,
    relaunch_requested: bool,
    previous_bundle_retained: bool,
}

pub(crate) enum InstallOutcome<Staged> {
    Cancelled {
        version: String,
        staged: Staged,
        resume_error: Option<String>,
    },
    Failed {
        version: String,
        message: String,
        staged: Option<Staged>,
        resume_error: Option<String>,
    },
    RelaunchRequested {
        version: String,
        previous_bundle: PathBuf,
    },
}

/// The host operations needed by the install state machine. The production
/// adapter uses the service barrier and updater; tests use a fake port so they
/// never inspect or modify a user's installed application.
pub(crate) trait UpdateInstallBackend: Send {
    type Staged: Send + 'static;

    fn current_bundle(&mut self) -> Result<PathBuf, String>;
    fn prepare(
        &mut self,
    ) -> impl Future<Output = Result<(), crate::service::UpdateQuiescenceError>> + Send;
    fn finalize_stop(
        &mut self,
    ) -> impl Future<Output = Result<(), crate::service::UpdateQuiescenceError>> + Send;
    fn cancel(
        &mut self,
    ) -> impl Future<Output = Result<(), crate::service::UpdateQuiescenceError>> + Send;
    fn install(
        &mut self,
        staged: Self::Staged,
        bundle: PathBuf,
    ) -> impl Future<Output = Result<InstallReceipt, String>> + Send;
}

/// Run the confirmed install protocol without any GPUI values. Closing either
/// channel is treated as cancellation until the installer has been invoked.
pub(crate) async fn run_install_flow<Backend: UpdateInstallBackend>(
    backend: &mut Backend,
    staged: Backend::Staged,
    version: String,
    controls: smol::channel::Receiver<InstallControl>,
    events: smol::channel::Sender<InstallProgress>,
) -> InstallOutcome<Backend::Staged> {
    if events.try_send(InstallProgress::CheckingBundle).is_err() {
        return InstallOutcome::Cancelled {
            version,
            staged,
            resume_error: None,
        };
    }
    let bundle = match backend.current_bundle() {
        Ok(bundle) => bundle,
        Err(message) => {
            return InstallOutcome::Failed {
                version,
                message,
                staged: Some(staged),
                resume_error: None,
            };
        }
    };

    loop {
        if events.try_send(InstallProgress::Preparing).is_err() {
            let resume_error = cancel_install_backend(backend, &events).await;
            return InstallOutcome::Cancelled {
                version,
                staged,
                resume_error,
            };
        }
        let preparation = {
            let prepare = backend.prepare();
            tokio::select! {
                result = prepare => Ok(result),
                control = controls.recv() => Err(control),
            }
        };
        let result = match preparation {
            Ok(result) => result,
            Err(Ok(InstallControl::Retry)) => continue,
            Err(Ok(InstallControl::Cancel | InstallControl::Commit) | Err(_)) => {
                let resume_error = cancel_install_backend(backend, &events).await;
                return InstallOutcome::Cancelled {
                    version,
                    staged,
                    resume_error,
                };
            }
        };
        match result {
            Ok(()) => match controls.try_recv() {
                Ok(InstallControl::Cancel | InstallControl::Commit)
                | Err(smol::channel::TryRecvError::Closed) => {
                    let resume_error = cancel_install_backend(backend, &events).await;
                    return InstallOutcome::Cancelled {
                        version,
                        staged,
                        resume_error,
                    };
                }
                Ok(InstallControl::Retry) | Err(smol::channel::TryRecvError::Empty) => break,
            },
            Err(crate::service::UpdateQuiescenceError::Busy) => {
                if events
                    .try_send(InstallProgress::WaitingForTransfers)
                    .is_err()
                {
                    let resume_error = cancel_install_backend(backend, &events).await;
                    return InstallOutcome::Cancelled {
                        version,
                        staged,
                        resume_error,
                    };
                }
                match controls.recv().await {
                    Ok(InstallControl::Retry) => {}
                    Ok(InstallControl::Cancel | InstallControl::Commit) | Err(_) => {
                        let resume_error = cancel_install_backend(backend, &events).await;
                        return InstallOutcome::Cancelled {
                            version,
                            staged,
                            resume_error,
                        };
                    }
                }
            }
            Err(error) => {
                let resume_error = cancel_install_backend(backend, &events).await;
                return InstallOutcome::Failed {
                    version,
                    message: format!("Could not prepare the receiver for updating: {error}."),
                    staged: Some(staged),
                    resume_error,
                };
            }
        }
    }

    if events.try_send(InstallProgress::StoppingListener).is_err() {
        let resume_error = cancel_install_backend(backend, &events).await;
        return InstallOutcome::Cancelled {
            version,
            staged,
            resume_error,
        };
    }
    if let Err(error) = backend.finalize_stop().await {
        let resume_error = cancel_install_backend(backend, &events).await;
        return InstallOutcome::Failed {
            version,
            message: format!("Could not stop the receiver before installing: {error}."),
            staged: Some(staged),
            resume_error,
        };
    }
    if events
        .try_send(InstallProgress::AwaitingInstallCommit)
        .is_err()
    {
        let resume_error = cancel_install_backend(backend, &events).await;
        return InstallOutcome::Cancelled {
            version,
            staged,
            resume_error,
        };
    }
    match controls.recv().await {
        Ok(InstallControl::Commit) => {}
        Ok(InstallControl::Cancel | InstallControl::Retry) | Err(_) => {
            let resume_error = cancel_install_backend(backend, &events).await;
            return InstallOutcome::Cancelled {
                version,
                staged,
                resume_error,
            };
        }
    }
    if !matches!(controls.try_recv(), Err(smol::channel::TryRecvError::Empty)) {
        let resume_error = cancel_install_backend(backend, &events).await;
        return InstallOutcome::Cancelled {
            version,
            staged,
            resume_error,
        };
    }
    if events.try_send(InstallProgress::Installing).is_err() {
        let resume_error = cancel_install_backend(backend, &events).await;
        return InstallOutcome::Cancelled {
            version,
            staged,
            resume_error,
        };
    }

    match backend.install(staged, bundle).await {
        Ok(receipt) if receipt.relaunch_requested && receipt.previous_bundle_retained => {
            let _ = events.try_send(InstallProgress::RelaunchRequested {
                version: receipt.version.clone(),
                previous_bundle: receipt.previous_bundle.clone(),
            });
            InstallOutcome::RelaunchRequested {
                version: receipt.version,
                previous_bundle: receipt.previous_bundle,
            }
        }
        Ok(_) => {
            let resume_error = cancel_install_backend(backend, &events).await;
            InstallOutcome::Failed {
                version: version.clone(),
                message: "The updater did not confirm a relaunch request and retained backup."
                    .to_string(),
                staged: None,
                resume_error,
            }
        }
        Err(message) => {
            let resume_error = cancel_install_backend(backend, &events).await;
            InstallOutcome::Failed {
                version,
                message,
                staged: None,
                resume_error,
            }
        }
    }
}

async fn cancel_install_backend<Backend: UpdateInstallBackend>(
    backend: &mut Backend,
    events: &smol::channel::Sender<InstallProgress>,
) -> Option<String> {
    let _ = events.try_send(InstallProgress::Resuming);
    backend
        .cancel()
        .await
        .err()
        .map(|error| format!("The receiver could not be resumed: {error}."))
}

struct ServiceInstallBackend {
    service: Arc<ServiceHandle>,
    updater: crate::update::UpdateService,
}

impl UpdateInstallBackend for ServiceInstallBackend {
    type Staged = StagedUpdate;

    fn current_bundle(&mut self) -> Result<PathBuf, String> {
        current_running_bundle()
    }

    fn prepare(
        &mut self,
    ) -> impl Future<Output = Result<(), crate::service::UpdateQuiescenceError>> + Send {
        let service = self.service.clone();
        async move { service.prepare_update_quiescence().await }
    }

    fn finalize_stop(
        &mut self,
    ) -> impl Future<Output = Result<(), crate::service::UpdateQuiescenceError>> + Send {
        let service = self.service.clone();
        async move { service.finalize_update_stop().await }
    }

    fn cancel(
        &mut self,
    ) -> impl Future<Output = Result<(), crate::service::UpdateQuiescenceError>> + Send {
        let service = self.service.clone();
        async move { service.cancel_update_quiescence().await }
    }

    fn install(
        &mut self,
        staged: Self::Staged,
        bundle: PathBuf,
    ) -> impl Future<Output = Result<InstallReceipt, String>> + Send {
        let updater = &self.updater;
        async move {
            let result = updater
                .install_after_confirmation(
                    staged,
                    &bundle,
                    crate::update::InstallConfirmation::user_confirmed(),
                )
                .map_err(|error| error.to_string())?;
            let retained = fs::symlink_metadata(&result.previous_bundle)
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
            Ok(InstallReceipt {
                version: result.version.to_string(),
                previous_bundle: result.previous_bundle,
                relaunch_requested: result.relaunch_requested,
                previous_bundle_retained: retained,
            })
        }
    }
}

/// Resolve and validate the *currently running* app bundle. The updater's own
/// destination checks are still applied immediately before replacement.
fn current_running_bundle() -> Result<PathBuf, String> {
    if std::env::consts::OS != "macos" {
        return Err("App-bundle installation is supported only on macOS.".to_string());
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not locate the running executable: {error}"))?;
    resolve_running_bundle_from_executable(&executable)
}

fn resolve_running_bundle_from_executable(executable: &std::path::Path) -> Result<PathBuf, String> {
    let executable = fs::canonicalize(executable)
        .map_err(|error| format!("Could not resolve the running executable: {error}"))?;
    let macos_dir = executable
        .parent()
        .filter(|path| path.file_name() == Some(OsStr::new("MacOS")))
        .ok_or_else(|| "This process is not running from a valid app bundle.".to_string())?;
    let contents = macos_dir
        .parent()
        .filter(|path| path.file_name() == Some(OsStr::new("Contents")))
        .ok_or_else(|| "This process is not running from a valid app bundle.".to_string())?;
    let bundle = contents
        .parent()
        .filter(|path| path.file_name() == Some(OsStr::new(crate::bundle::BUNDLE_NAME)))
        .ok_or_else(|| "Standalone builds cannot install app-bundle updates.".to_string())?;

    let bundle_metadata = fs::symlink_metadata(bundle)
        .map_err(|_| "The running app bundle could not be inspected.".to_string())?;
    if !bundle_metadata.is_dir() || bundle_metadata.file_type().is_symlink() {
        return Err("The running app bundle is not a real directory.".to_string());
    }
    let canonical_bundle = fs::canonicalize(bundle)
        .map_err(|_| "The running app bundle could not be resolved.".to_string())?;
    let expected_executable = canonical_bundle
        .join("Contents")
        .join("MacOS")
        .join(crate::bundle::EXECUTABLE);
    let executable_metadata = fs::symlink_metadata(&expected_executable)
        .map_err(|_| "The running app bundle executable is missing.".to_string())?;
    if !executable_metadata.is_file() || executable_metadata.file_type().is_symlink() {
        return Err("The running app bundle executable is not a regular file.".to_string());
    }
    if fs::canonicalize(&expected_executable).ok().as_deref() != Some(executable.as_path()) {
        return Err(
            "The current executable is not the exact executable in this app bundle.".to_string(),
        );
    }

    let info_plist = canonical_bundle.join("Contents").join("Info.plist");
    let info_metadata = fs::symlink_metadata(&info_plist)
        .map_err(|_| "The running app bundle Info.plist is missing.".to_string())?;
    if !info_metadata.is_file() || info_metadata.file_type().is_symlink() {
        return Err("The running app bundle Info.plist is not a regular file.".to_string());
    }
    let plist = plist::Value::from_file(&info_plist)
        .map_err(|_| "The running app bundle Info.plist is invalid.".to_string())?;
    let dictionary = plist
        .as_dictionary()
        .ok_or_else(|| "The running app bundle Info.plist is invalid.".to_string())?;
    let matches = |key: &str, expected: &str| {
        dictionary.get(key).and_then(plist::Value::as_string) == Some(expected)
    };
    let version = env!("CARGO_PKG_VERSION");
    if !matches("CFBundleIdentifier", crate::bundle::BUNDLE_ID)
        || !matches("CFBundleExecutable", crate::bundle::EXECUTABLE)
        || !matches("CFBundlePackageType", "APPL")
        || !matches("CFBundleShortVersionString", version)
        || !matches("CFBundleVersion", version)
    {
        return Err(
            "The running app bundle identity or version does not match this build.".to_string(),
        );
    }
    Ok(canonical_bundle)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Send,
    Receive,
    History,
    Settings,
}

impl Phase {
    fn title(self) -> &'static str {
        match self {
            Phase::Send => "Send",
            Phase::Receive => "Receive",
            Phase::History => "History",
            Phase::Settings => "Settings",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Phase::Send => IconName::Send,
            Phase::Receive => IconName::Inbox,
            Phase::History => IconName::Clock,
            Phase::Settings => IconName::Settings,
        }
    }
}

pub struct DesktopApp {
    pub(crate) phase: Phase,
    pub(crate) store: Entity<TransferStore>,
    /// The service runtime, kept alive for the life of the window.
    _runtime: Option<HostRuntime>,
    /// The settings last applied (what "Revert" restores).
    pub(crate) settings: Settings,
    pub(crate) settings_form: Entity<SettingsForm>,
    pub(crate) selected_peer: Option<String>,
    pub(crate) staged: Vec<crate::service::SendFile>,
    pub(crate) update_state: UpdateState,
    pub(crate) install_state: InstallState,
    install_controls: Option<smol::channel::Sender<InstallControl>>,
    pub(crate) pin_input: Entity<gpui_kit::component::input::InputState>,
    sidebar_visible: bool,
    _subscriptions: Vec<Subscription>,
}

impl DesktopApp {
    /// The real app: load settings, start the tokio service, wire the store to it.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = Settings::load();
        let (store, runtime, problem) = match HostRuntime::new() {
            Ok(runtime) => {
                let handler = ServiceHandle::spawn(&runtime.handle(), settings.clone());
                (
                    cx.new(|cx| TransferStore::new(handler, cx)),
                    Some(runtime),
                    None,
                )
            }
            Err(message) => (cx.new(|_| TransferStore::detached()), None, Some(message)),
        };
        let start_update_check = runtime.is_some();
        let mut app = Self::assemble(settings, store, runtime, window, cx);
        if let Some(message) = problem {
            app.store.update(cx, |store, cx| {
                store.apply_event(
                    ServiceEvent::Notice {
                        level: NoticeLevel::Error,
                        message,
                    },
                    cx,
                );
            });
        }
        // Discovery runs on HostRuntime and reports back through GPUI's async
        // channel, so it does not delay first paint or receiver startup.
        schedule_startup_update_check(start_update_check, || app.check_for_updates(cx));
        app
    }

    /// A shell with no service behind it: used by the UI tests, which feed events
    /// in by hand and read the commands back.
    pub fn detached(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = Settings::default();
        let store = cx.new(|_| TransferStore::detached());
        Self::assemble(settings, store, None, window, cx)
    }

    fn assemble(
        settings: Settings,
        store: Entity<TransferStore>,
        runtime: Option<HostRuntime>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        theme::apply_surface_opacity(
            if settings.glass_enabled {
                settings.surface_opacity
            } else {
                MAX_SURFACE_OPACITY
            },
            cx,
        );
        window
            .set_background_appearance(theme::window_background_appearance(settings.glass_enabled));
        let settings_form = cx.new(|cx| SettingsForm::new(&settings, window, cx));
        let pin_input =
            cx.new(|cx| gpui_kit::component::input::InputState::new(window, cx).placeholder("PIN"));
        let subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.observe(&settings_form, |_, _, cx| cx.notify()),
        ];
        Self {
            phase: Phase::Receive,
            store,
            _runtime: runtime,
            settings,
            settings_form,
            selected_peer: None,
            staged: Vec::new(),
            update_state: UpdateState::Idle,
            install_state: InstallState::Idle,
            install_controls: None,
            pin_input,
            sidebar_visible: true,
            _subscriptions: subscriptions,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn store(&self) -> &Entity<TransferStore> {
        &self.store
    }

    /// The settings draft. Public so a test can drive a field the way a person
    /// would, then press Apply.
    pub fn settings_form(&self) -> &Entity<SettingsForm> {
        &self.settings_form
    }

    /// Files staged for the next send.
    pub fn staged(&self) -> &[crate::service::SendFile] {
        &self.staged
    }

    pub(crate) fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.update_state,
            UpdateState::Checking | UpdateState::Staging(_)
        ) {
            return;
        }
        let Some(runtime) = self._runtime.as_ref() else {
            self.update_state = UpdateState::Failed(
                "Update checks are unavailable in a detached session.".to_string(),
            );
            cx.notify();
            return;
        };

        self.update_state = UpdateState::Checking;
        self.install_state = InstallState::Idle;
        self.install_controls = None;
        let (sender, receiver) = smol::channel::bounded(1);
        runtime.spawn(async move {
            let result = async {
                let updater =
                    crate::update::UpdateService::new().map_err(|error| error.to_string())?;
                updater
                    .discover_update()
                    .await
                    .map_err(|error| error.to_string())
            }
            .await;
            let _ = sender.send(result).await;
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = receiver.recv().await else {
                return;
            };
            let _ = this.update(cx, |app, cx| {
                app.update_state = match result {
                    Ok(Some(release)) => UpdateState::Available(release),
                    Ok(None) => UpdateState::UpToDate,
                    Err(message) => UpdateState::Failed(message),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Open the explicit confirmation step; no download or staging happens here.
    pub(crate) fn request_update_download(&mut self, cx: &mut Context<Self>) {
        let UpdateState::Available(release) = &self.update_state else {
            return;
        };
        self.update_state = UpdateState::ConfirmingDownload(release.clone());
        cx.notify();
    }

    pub(crate) fn cancel_update_download(&mut self, cx: &mut Context<Self>) {
        let UpdateState::ConfirmingDownload(release) = &self.update_state else {
            return;
        };
        self.update_state = UpdateState::Available(release.clone());
        cx.notify();
    }

    /// Start download and verification only from the affirmative confirmation
    /// action. All network, extraction and signature work stays off the GUI thread.
    pub(crate) fn confirm_update_download(&mut self, cx: &mut Context<Self>) {
        let UpdateState::ConfirmingDownload(release) = &self.update_state else {
            return;
        };
        let release = release.clone();
        if !UPDATE_SIGNING_TRUST_CONFIGURED {
            self.update_state = UpdateState::Failed(update_signing_trust_message().to_string());
            cx.notify();
            return;
        }
        let Some(runtime) = self._runtime.as_ref() else {
            self.update_state = UpdateState::Failed(
                "Update downloads are unavailable in a detached session.".to_string(),
            );
            cx.notify();
            return;
        };

        self.update_state = UpdateState::Staging(release.clone());
        let (sender, receiver) = smol::channel::bounded(1);
        let release_for_staging = release.clone();
        runtime.spawn(async move {
            let result = async {
                let updater =
                    crate::update::UpdateService::new().map_err(|error| error.to_string())?;
                let update = updater
                    .stage_release(&release_for_staging, &std::env::temp_dir())
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(update)
            }
            .await;
            let _ = sender.send(result).await;
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = receiver.recv().await else {
                return;
            };
            let _ = this.update(cx, |app, cx| {
                app.update_state = match result {
                    Ok(update) => UpdateState::Staged { release, update },
                    Err(message) => UpdateState::Failed(message),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn request_update_install(&mut self, cx: &mut Context<Self>) {
        if request_install_confirmation(
            &mut self.install_state,
            matches!(self.update_state, UpdateState::Staged { .. }),
        ) {
            cx.notify();
        }
    }

    pub(crate) fn cancel_update_install_confirmation(&mut self, cx: &mut Context<Self>) {
        if cancel_install_confirmation(&mut self.install_state) {
            cx.notify();
        }
    }

    pub(crate) fn confirm_update_install(&mut self, cx: &mut Context<Self>) {
        if self.install_state != InstallState::Confirming {
            return;
        }
        if !UPDATE_SIGNING_TRUST_CONFIGURED {
            self.install_state = InstallState::Failed {
                version: staged_version(&self.update_state)
                    .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
                message: update_signing_trust_message().to_string(),
                resume_error: None,
            };
            cx.notify();
            return;
        }

        let Some(runtime) = self._runtime.as_ref().map(HostRuntime::handle) else {
            self.install_state = InstallState::Failed {
                version: staged_version(&self.update_state)
                    .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
                message: "The application runtime is unavailable.".to_string(),
                resume_error: None,
            };
            cx.notify();
            return;
        };
        let service = {
            let store = self.store.read(cx);
            if !store.is_live() || !store.session().status.is_listening() {
                self.install_state = InstallState::Failed {
                    version: staged_version(&self.update_state)
                        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
                    message:
                        "The receiver controller is not listening; installation was not started."
                            .to_string(),
                    resume_error: None,
                };
                cx.notify();
                return;
            }
            let Some(service) = store.service_handle() else {
                self.install_state = InstallState::Failed {
                    version: staged_version(&self.update_state)
                        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
                    message:
                        "The receiver controller is unavailable; installation was not started."
                            .to_string(),
                    resume_error: None,
                };
                cx.notify();
                return;
            };
            service
        };
        let previous_state = std::mem::replace(&mut self.update_state, UpdateState::Idle);
        let UpdateState::Staged { release, update } = previous_state else {
            self.update_state = previous_state;
            return;
        };
        let version = update.version().to_string();
        self.update_state = UpdateState::Installing(release.clone());
        self.install_state = InstallState::Preparing;

        let (progress_sender, progress_receiver) = smol::channel::unbounded();
        let (control_sender, control_receiver) = smol::channel::unbounded();
        let (completion_sender, completion_receiver) = smol::channel::bounded(1);
        self.install_controls = Some(control_sender.clone());

        runtime.spawn(async move {
            let outcome = match crate::update::UpdateService::new() {
                Ok(updater) => {
                    let mut backend = ServiceInstallBackend { service, updater };
                    run_install_flow(
                        &mut backend,
                        update,
                        version,
                        control_receiver,
                        progress_sender,
                    )
                    .await
                }
                Err(error) => {
                    let _ = progress_sender.try_send(InstallProgress::CheckingBundle);
                    InstallOutcome::Failed {
                        version,
                        message: error.to_string(),
                        staged: Some(update),
                        resume_error: None,
                    }
                }
            };
            let _ = completion_sender.send(outcome).await;
        });

        cx.spawn(async move |this, cx| {
            while let Ok(progress) = progress_receiver.recv().await {
                if this
                    .update(cx, |app, cx| app.apply_install_progress(progress, cx))
                    .is_err()
                {
                    break;
                }
            }
            let Ok(outcome) = completion_receiver.recv().await else {
                return;
            };
            let relaunched = matches!(outcome, InstallOutcome::RelaunchRequested { .. });
            let _ = this.update(cx, |app, cx| {
                app.finish_update_install(outcome, release, cx)
            });
            if relaunched {
                smol::Timer::after(std::time::Duration::from_millis(900)).await;
                cx.update(|cx| cx.quit());
            }
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn retry_update_install(&mut self, cx: &mut Context<Self>) {
        if self.install_state == InstallState::WaitingForTransfers {
            if self
                .install_controls
                .as_ref()
                .is_some_and(|controls| controls.try_send(InstallControl::Retry).is_ok())
            {
                self.install_state = InstallState::Preparing;
            } else {
                self.install_state = InstallState::Failed {
                    version: staged_version(&self.update_state)
                        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
                    message: "The update task is no longer available.".to_string(),
                    resume_error: None,
                };
            }
            cx.notify();
        }
    }

    pub(crate) fn cancel_update_install(&mut self, cx: &mut Context<Self>) {
        match self.install_state {
            InstallState::Confirming => {
                self.install_state = InstallState::Idle;
            }
            InstallState::Preparing
            | InstallState::WaitingForTransfers
            | InstallState::StoppingListener => {
                let sent = self
                    .install_controls
                    .as_ref()
                    .is_some_and(|controls| controls.try_send(InstallControl::Cancel).is_ok());
                if sent {
                    self.install_state = InstallState::Resuming;
                } else {
                    self.install_state = InstallState::Failed {
                        version: staged_version(&self.update_state)
                            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
                        message: "The update task is no longer available.".to_string(),
                        resume_error: None,
                    };
                }
            }
            _ => return,
        }
        cx.notify();
    }

    fn apply_install_progress(&mut self, progress: InstallProgress, cx: &mut Context<Self>) {
        if progress == InstallProgress::AwaitingInstallCommit {
            self.install_state = InstallState::Installing;
            if let Some(controls) = self.install_controls.as_ref() {
                let _ = controls.try_send(InstallControl::Commit);
            }
            cx.notify();
            return;
        }
        self.install_state = match progress {
            InstallProgress::CheckingBundle | InstallProgress::Preparing => InstallState::Preparing,
            InstallProgress::WaitingForTransfers => InstallState::WaitingForTransfers,
            InstallProgress::Resuming => InstallState::Resuming,
            InstallProgress::StoppingListener => InstallState::StoppingListener,
            InstallProgress::AwaitingInstallCommit => unreachable!("handled before matching"),
            InstallProgress::Installing => InstallState::Installing,
            InstallProgress::RelaunchRequested {
                version,
                previous_bundle,
            } => InstallState::RelaunchRequested {
                version,
                previous_bundle,
            },
        };
        cx.notify();
    }

    fn finish_update_install(
        &mut self,
        outcome: InstallOutcome<StagedUpdate>,
        release: ReleaseInfo,
        cx: &mut Context<Self>,
    ) {
        self.install_controls = None;
        match outcome {
            InstallOutcome::Cancelled {
                version,
                staged,
                resume_error,
            } => {
                self.update_state = UpdateState::Staged {
                    release,
                    update: staged,
                };
                self.install_state = match resume_error {
                    Some(message) => InstallState::Failed {
                        version,
                        message: "Installation was cancelled.".to_string(),
                        resume_error: Some(message),
                    },
                    None => InstallState::Idle,
                };
            }
            InstallOutcome::Failed {
                version,
                message,
                staged,
                resume_error,
            } => {
                self.update_state = match staged {
                    Some(update) => UpdateState::Staged { release, update },
                    None => UpdateState::Failed(format!("Version {version}: {message}")),
                };
                self.install_state = InstallState::Failed {
                    version,
                    message,
                    resume_error,
                };
            }
            InstallOutcome::RelaunchRequested {
                version,
                previous_bundle,
            } => {
                self.install_state = InstallState::RelaunchRequested {
                    version,
                    previous_bundle,
                };
            }
        }
        cx.notify();
    }

    // ------------------------------------------------------------------- actions

    pub fn select_phase(&mut self, phase: Phase, cx: &mut Context<Self>) {
        self.phase = phase;
        cx.notify();
    }

    pub fn select_peer(&mut self, node_id: String, cx: &mut Context<Self>) {
        self.selected_peer = Some(node_id);
        cx.notify();
    }

    pub fn unstage(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.staged.len() {
            self.staged.remove(index);
            cx.notify();
        }
    }

    pub fn add_files(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        for file in views::send::staged_files(paths) {
            if !self.staged.iter().any(|staged| staged.path == file.path) {
                self.staged.push(file);
            }
        }
        cx.notify();
    }

    /// The OS file panel. `rfd`'s async dialog is safe to await on the foreground
    /// executor: it opens on the main thread and returns through its own channel.
    pub fn pick_files(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Choose files to send")
                .pick_files()
                .await;
            if let Some(handles) = picked {
                let paths: Vec<PathBuf> = handles
                    .into_iter()
                    .map(|handle| handle.path().to_path_buf())
                    .collect();
                let _ = this.update(cx, |app, cx| app.add_files(paths, cx));
            }
        })
        .detach();
    }

    pub fn pick_save_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The folder goes into an `InputState`, which needs the window, so this
        // variant of `spawn` is the one that carries it across the await.
        cx.spawn_in(window, async move |this, cx| {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Choose where received files go")
                .pick_folder()
                .await;
            let Some(handle) = picked else {
                return;
            };
            let path = handle.path().to_path_buf();
            let _ = cx.update(|window, app| {
                let _ = this.update(app, |app, cx| {
                    app.settings_form.update(cx, |form, cx| {
                        form.set_save_dir(&path, window, cx);
                        form.saved = false;
                    });
                    cx.notify();
                });
            });
        })
        .detach();
    }

    pub fn start_send(&mut self, cx: &mut Context<Self>) {
        let Some(node_id) = self.selected_peer.clone() else {
            return;
        };
        let session = self.store.read(cx).session();
        let Some(peer) = session.peer(&node_id).cloned() else {
            return;
        };
        if self.staged.is_empty() {
            return;
        }
        let files: Vec<PathBuf> = self.staged.iter().map(|file| file.path.clone()).collect();
        let pin = self.pin_input.read(cx).value().trim().to_string();
        let pin = (!pin.is_empty()).then_some(pin);
        self.store.read(cx).send(peer, files, pin);
        self.staged.clear();
        cx.notify();
    }

    /// Star or unstar a device. Identity is the fingerprint, so this survives the
    /// phone getting a new address; the change is saved immediately and pushed to
    /// the receiver live, with no listener restart.
    pub fn toggle_favorite(&mut self, fingerprint: &str, cx: &mut Context<Self>) {
        let now_favorite = self.settings.toggle_favorite(fingerprint);
        self.settings.normalize_in_place();
        let live = self.store.read(cx).is_live();
        if live {
            let _ = self.settings.save();
        }
        self.store
            .read(cx)
            .set_favorites(self.settings.favorites.clone());
        self.settings_form.update(cx, |form, _| {
            form.error = None;
            form.saved = false;
        });
        let alias = self
            .store
            .read(cx)
            .session()
            .peers
            .iter()
            .find(|peer| peer.node_id == fingerprint)
            .map(|peer| peer.alias.clone())
            .unwrap_or_else(|| "Device".to_string());
        self.store.update(cx, |store, cx| {
            store.apply_event(
                ServiceEvent::Notice {
                    level: NoticeLevel::Info,
                    message: if now_favorite {
                        format!("{alias} is a favorite — quick save can accept from it.")
                    } else {
                        format!("{alias} is no longer a favorite.")
                    },
                },
                cx,
            );
        });
        cx.notify();
    }

    pub fn is_favorite(&self, fingerprint: &str) -> bool {
        self.settings.is_favorite(fingerprint)
    }

    /// Restore the last applied settings and their appearance preview.
    pub fn revert_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.settings.clone();
        self.settings_form
            .update(cx, |form, cx| form.reload(&settings, window, cx));
        theme::apply_surface_opacity(
            if settings.glass_enabled {
                settings.surface_opacity
            } else {
                MAX_SURFACE_OPACITY
            },
            cx,
        );
        window
            .set_background_appearance(theme::window_background_appearance(settings.glass_enabled));
        cx.notify();
    }

    pub fn apply_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.settings_form.read(cx).draft(&self.settings, cx);
        let settings = match draft {
            Ok(settings) => settings,
            Err(message) => {
                self.settings_form.update(cx, |form, _| {
                    form.error = Some(message);
                    form.saved = false;
                });
                cx.notify();
                return;
            }
        };

        let live = self.store.read(cx).is_live();
        if live {
            if let Err(error) = settings.save() {
                self.settings_form.update(cx, |form, _| {
                    form.error = Some(format!("Could not save the settings file: {error}"));
                    form.saved = false;
                });
                cx.notify();
                return;
            }
        }

        let receiver_changed = self.settings.receiver_config_changed(&settings);
        let started = if receiver_changed {
            self.store.read(cx).apply_settings(settings.clone())
        } else {
            live
        };
        theme::apply_surface_opacity(
            if settings.glass_enabled {
                settings.surface_opacity
            } else {
                MAX_SURFACE_OPACITY
            },
            cx,
        );
        window
            .set_background_appearance(theme::window_background_appearance(settings.glass_enabled));
        cx.refresh_windows();
        self.settings = settings;
        self.settings_form.update(cx, |form, _| {
            form.error = if !live {
                Some(if receiver_changed {
                    "Receiver settings were not saved or applied because the network service is unavailable; appearance changes are active for this session.".to_string()
                } else {
                    "Appearance changes are active only for this session; settings were not saved because the network service is unavailable.".to_string()
                })
            } else if !started {
                Some("Receiver settings could not be applied because the network service stopped.".to_string())
            } else {
                None
            };
            form.saved = live && started;
        });
        cx.notify();
    }

    // ------------------------------------------------------------------- drawing

    fn render_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let session = self.store.read(cx).session();
        let alias = match &session.status {
            ServerStatus::Listening { alias, .. } => alias.clone(),
            _ => "This device".to_string(),
        };
        let (dot, label) = match &session.status {
            ServerStatus::Listening { port, .. } => {
                (theme::success(cx), format!("Listening · {port}"))
            }
            ServerStatus::Starting => (theme::warning(cx), "Starting…".to_string()),
            ServerStatus::Failed { .. } => (theme::danger(cx), "Receiver offline".to_string()),
            ServerStatus::Stopped => (cx.theme().muted_foreground, "Stopped".to_string()),
        };

        TitleBar::new()
            .w_full()
            .h(s(theme::TOOLBAR_HEIGHT))
            .bg(theme::toolbar(cx))
            .child(
                h_flex()
                    .w(s(theme::SIDEBAR_WIDTH))
                    .h_full()
                    .flex_shrink_0()
                    .pl(s(18.))
                    .child(
                        Button::new("toggle-sidebar")
                            .ghost()
                            .small()
                            .accessibility_label("Toggle sidebar")
                            .icon(IconName::PanelLeft)
                            .tooltip("Toggle sidebar")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sidebar_visible = !this.sidebar_visible;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .items_center()
                    .px(s(16.))
                    .gap(s(9.))
                    .child(
                        v_flex()
                            .gap(s(1.))
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(s(14.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(self.phase.title()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(s(11.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(alias),
                            ),
                    )
                    .child(div().flex_1().h_full())
                    .child(
                        h_flex()
                            .gap(s(6.))
                            .px(s(9.))
                            .py(s(4.))
                            .rounded_full()
                            .bg(cx.theme().secondary.alpha(theme::surface_opacity()))
                            .child(views::status_dot(dot))
                            .child(
                                div()
                                    .text_size(s(11.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(label),
                            ),
                    )
                    .child(theme::theme_toggle_button(cx)),
            )
    }

    fn render_sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
        let session = self.store.read(cx).session();
        let peers = session.peers.len();
        let offers = session.offers.len();
        let history = session.history.len();
        let (dot, alias, detail) = match &session.status {
            ServerStatus::Listening {
                alias,
                protocol,
                fingerprint,
                ..
            } => (
                theme::success(cx),
                alias.clone(),
                format!(
                    "Listening · {} · {}",
                    protocol.as_str().to_uppercase(),
                    crate::service::format::short_fingerprint(fingerprint)
                ),
            ),
            ServerStatus::Starting => (
                theme::warning(cx),
                "This device".to_string(),
                "Starting…".to_string(),
            ),
            ServerStatus::Failed { .. } => (
                theme::danger(cx),
                "This device".to_string(),
                "Receiver offline".to_string(),
            ),
            ServerStatus::Stopped => (
                cx.theme().muted_foreground,
                "This device".to_string(),
                "Not listening".to_string(),
            ),
        };

        let items = [
            (Phase::Send, Some(peers)),
            (Phase::Receive, Some(offers)),
            (Phase::History, Some(history)),
            (Phase::Settings, None),
        ]
        .into_iter()
        .map(|(phase, count)| {
            let active = self.phase == phase;
            let mut item = SidebarMenuItem::new(phase.title())
                .icon(Icon::new(phase.icon()))
                .active(active)
                .accessibility_label(phase.title())
                .on_click(cx.listener(move |this, _, _, cx| this.select_phase(phase, cx)));
            if let Some(count) = count.filter(|count| *count > 0) {
                item = item.suffix(move |_, cx| {
                    div()
                        .px(s(6.))
                        .rounded_full()
                        .bg(if phase == Phase::Receive {
                            cx.theme().warning
                        } else {
                            cx.theme().border
                        })
                        .text_size(s(10.))
                        .child(count.to_string())
                });
            }
            item
        });

        let header = h_flex()
            .w_full()
            .gap(s(9.))
            .items_center()
            .child(
                h_flex()
                    .size(s(26.))
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .child(img(branding::sidebar_mark(cx.theme().mode.is_dark())).size(s(26.))),
            )
            .child(
                h_flex()
                    .min_w_0()
                    .items_center()
                    .gap(s(6.))
                    .child(
                        div()
                            .text_size(s(14.))
                            .font_weight(FontWeight::BOLD)
                            .child("LocalSend"),
                    )
                    .child(
                        div()
                            .px(s(4.))
                            .py(s(1.))
                            .rounded(s(4.))
                            .bg(cx.theme().border)
                            .text_size(s(9.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(cx.theme().muted_foreground)
                            .child("RS"),
                    ),
            );
        let footer = v_flex()
            .mx(s(10.))
            .mb(s(12.))
            .p(s(10.))
            .gap(s(6.))
            .rounded(s(9.))
            .bg(theme::card(cx))
            .border_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .gap(s(7.))
                    .child(views::status_dot(dot))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(s(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(alias),
                    ),
            )
            .child(
                div()
                    .text_size(s(10.))
                    .text_color(cx.theme().muted_foreground)
                    .child(detail),
            );

        Sidebar::new("main-sidebar")
            .w(s(theme::SIDEBAR_WIDTH))
            .h_full()
            .collapsible(SidebarCollapsible::None)
            .bg(theme::sidebar(cx))
            .header(header)
            .child(SidebarGroup::new("TRANSFER").child(SidebarMenu::new().children(items)))
            .footer(footer)
    }

    fn render_notice(&self, cx: &Context<Self>) -> Option<Div> {
        let session = self.store.read(cx).session();
        let notice = session.last_notice()?;
        let color = match notice.level {
            NoticeLevel::Error => theme::danger(cx),
            NoticeLevel::Warning => theme::warning(cx),
            NoticeLevel::Info => cx.theme().primary,
        };
        let icon = match notice.level {
            NoticeLevel::Error => IconName::CircleAlert,
            NoticeLevel::Warning => IconName::OctagonAlert,
            NoticeLevel::Info => IconName::Info,
        };
        Some(
            h_flex()
                .w_full()
                .flex_shrink_0()
                .px(s(16.))
                .py(s(8.))
                .gap(s(8.))
                .bg(color.opacity(0.10))
                .border_b_1()
                .border_color(color.opacity(0.25))
                .child(Icon::new(icon).size(s(14.)).text_color(color))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(s(12.))
                        .text_color(color)
                        .child(notice.message.clone()),
                )
                .child(
                    Button::new("dismiss-notice")
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::X).size(s(11.)))
                        .tooltip("Dismiss")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.store.update(cx, |store, cx| {
                                store.session.dismiss_notice();
                                cx.notify();
                            });
                            cx.notify();
                        })),
                ),
        )
    }
}

impl Render for DesktopApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.phase {
            Phase::Send => views::send::render(self, cx),
            Phase::Receive => views::receive::render(self, cx),
            Phase::History => views::history::render(self, cx),
            Phase::Settings => views::settings::render(self, cx),
        };
        let notice = self.render_notice(cx);
        let sidebar = self.sidebar_visible.then(|| self.render_sidebar(cx));

        v_flex()
            .size_full()
            .text_color(cx.theme().foreground)
            .text_size(s(13.))
            .child(self.render_toolbar(cx))
            .children(notice)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .items_stretch()
                    .children(sidebar)
                    .child(v_flex().flex_1().min_w_0().min_h_0().h_full().child(body)),
            )
    }
}

#[cfg(test)]
mod update_install_flow_tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use super::*;

    struct FakeInstallBackend {
        prepares: VecDeque<Result<(), crate::service::UpdateQuiescenceError>>,
        finalize: Result<(), crate::service::UpdateQuiescenceError>,
        finalize_gate: Option<tokio::sync::oneshot::Receiver<()>>,
        finalize_entered: Option<tokio::sync::oneshot::Sender<()>>,
        cancel: Result<(), crate::service::UpdateQuiescenceError>,
        install: Result<InstallReceipt, String>,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl UpdateInstallBackend for FakeInstallBackend {
        type Staged = String;

        fn current_bundle(&mut self) -> Result<PathBuf, String> {
            self.calls.lock().unwrap().push("bundle");
            Ok(PathBuf::from("/fake/LocalSend RS.app"))
        }

        fn prepare(
            &mut self,
        ) -> impl std::future::Future<Output = Result<(), crate::service::UpdateQuiescenceError>> + Send
        {
            self.calls.lock().unwrap().push("prepare");
            let result = self.prepares.pop_front().unwrap();
            async move { result }
        }

        fn finalize_stop(
            &mut self,
        ) -> impl std::future::Future<Output = Result<(), crate::service::UpdateQuiescenceError>> + Send
        {
            self.calls.lock().unwrap().push("finalize");
            let result = self.finalize;
            if let Some(entered) = self.finalize_entered.take() {
                let _ = entered.send(());
            }
            let gate = self.finalize_gate.take();
            async move {
                if let Some(gate) = gate {
                    let _ = gate.await;
                }
                result
            }
        }

        fn cancel(
            &mut self,
        ) -> impl std::future::Future<Output = Result<(), crate::service::UpdateQuiescenceError>> + Send
        {
            self.calls.lock().unwrap().push("cancel");
            let result = self.cancel;
            async move { result }
        }

        fn install(
            &mut self,
            staged: Self::Staged,
            _bundle: PathBuf,
        ) -> impl std::future::Future<Output = Result<InstallReceipt, String>> + Send {
            assert_eq!(staged, "verified-stage");
            self.calls.lock().unwrap().push("install");
            let result = self.install.clone();
            async move { result }
        }
    }

    fn fake_backend(
        prepares: impl IntoIterator<Item = Result<(), crate::service::UpdateQuiescenceError>>,
        install: Result<InstallReceipt, String>,
    ) -> (FakeInstallBackend, Arc<Mutex<Vec<&'static str>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            FakeInstallBackend {
                prepares: prepares.into_iter().collect(),
                finalize: Ok(()),
                finalize_gate: None,
                finalize_entered: None,
                cancel: Ok(()),
                install,
                calls: calls.clone(),
            },
            calls,
        )
    }

    fn successful_receipt() -> InstallReceipt {
        InstallReceipt {
            version: "9.9.9".to_string(),
            previous_bundle: PathBuf::from("/fake/previous.app"),
            relaunch_requested: true,
            previous_bundle_retained: true,
        }
    }

    #[::core::prelude::v1::test]
    fn install_confirmation_requires_a_staged_update_and_cancel_is_non_destructive() {
        let mut state = InstallState::Idle;
        assert!(!request_install_confirmation(&mut state, false));
        assert_eq!(state, InstallState::Idle);

        assert!(request_install_confirmation(&mut state, true));
        assert_eq!(state, InstallState::Confirming);
        assert!(cancel_install_confirmation(&mut state));
        assert_eq!(state, InstallState::Idle);
    }

    #[::core::prelude::v1::test]
    fn startup_check_is_scheduled_for_live_apps_but_not_detached_sessions() {
        let mut scheduled = false;
        schedule_startup_update_check(true, || scheduled = true);
        assert!(scheduled);

        scheduled = false;
        schedule_startup_update_check(false, || panic!("detached startup must not check online"));
        assert!(!scheduled);
    }

    #[::core::prelude::v1::test]
    fn exact_bundle_resolver_rejects_a_standalone_executable() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join(crate::bundle::EXECUTABLE);
        std::fs::write(&executable, b"standalone").unwrap();

        assert!(resolve_running_bundle_from_executable(&executable).is_err());
    }

    #[::core::prelude::v1::test]
    fn exact_bundle_resolver_accepts_only_the_running_executable_in_a_valid_app() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source-executable");
        std::fs::write(&source, b"fixture executable").unwrap();
        let bundle = crate::bundle::bundle(temp.path(), &source).unwrap();
        let executable = bundle
            .join("Contents")
            .join("MacOS")
            .join(crate::bundle::EXECUTABLE);

        assert_eq!(
            resolve_running_bundle_from_executable(&executable).unwrap(),
            std::fs::canonicalize(bundle).unwrap()
        );
    }

    #[tokio::test]
    async fn busy_wait_can_retry_before_stopping_and_installing() {
        let (mut backend, calls) = fake_backend(
            [Err(crate::service::UpdateQuiescenceError::Busy), Ok(())],
            Ok(successful_receipt()),
        );
        let (event_tx, event_rx) = smol::channel::unbounded();
        let (control_tx, control_rx) = smol::channel::unbounded();
        let task = tokio::spawn(async move {
            run_install_flow(
                &mut backend,
                "verified-stage".to_string(),
                "9.9.9".to_string(),
                control_rx,
                event_tx,
            )
            .await
        });

        assert_eq!(
            event_rx.recv().await.unwrap(),
            InstallProgress::CheckingBundle
        );
        assert_eq!(event_rx.recv().await.unwrap(), InstallProgress::Preparing);
        assert_eq!(
            event_rx.recv().await.unwrap(),
            InstallProgress::WaitingForTransfers
        );
        control_tx.send(InstallControl::Retry).await.unwrap();
        while event_rx.recv().await.unwrap() != InstallProgress::AwaitingInstallCommit {}
        control_tx.send(InstallControl::Commit).await.unwrap();
        let outcome = task.await.unwrap();

        assert!(matches!(
            outcome,
            InstallOutcome::RelaunchRequested { version, .. } if version == "9.9.9"
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            ["bundle", "prepare", "prepare", "finalize", "install"]
        );
    }

    #[tokio::test]
    async fn cancelling_while_busy_reopens_the_service_and_preserves_the_stage() {
        let (mut backend, calls) = fake_backend(
            [Err(crate::service::UpdateQuiescenceError::Busy)],
            Ok(successful_receipt()),
        );
        let (event_tx, event_rx) = smol::channel::unbounded();
        let (control_tx, control_rx) = smol::channel::unbounded();
        let task = tokio::spawn(async move {
            run_install_flow(
                &mut backend,
                "verified-stage".to_string(),
                "9.9.9".to_string(),
                control_rx,
                event_tx,
            )
            .await
        });

        while event_rx.recv().await.unwrap() != InstallProgress::WaitingForTransfers {}
        control_tx.send(InstallControl::Cancel).await.unwrap();
        let outcome = task.await.unwrap();

        assert!(matches!(
            outcome,
            InstallOutcome::Cancelled {
                staged,
                resume_error: None,
                ..
            } if staged == "verified-stage"
        ));
        assert_eq!(*calls.lock().unwrap(), ["bundle", "prepare", "cancel"]);
    }

    #[tokio::test]
    async fn failed_resume_is_reported_without_losing_the_staged_update() {
        let (mut backend, _) = fake_backend(
            [Err(crate::service::UpdateQuiescenceError::Busy)],
            Ok(successful_receipt()),
        );
        backend.cancel = Err(crate::service::UpdateQuiescenceError::Unavailable);
        let (event_tx, event_rx) = smol::channel::unbounded();
        let (control_tx, control_rx) = smol::channel::unbounded();
        let task = tokio::spawn(async move {
            run_install_flow(
                &mut backend,
                "verified-stage".to_string(),
                "9.9.9".to_string(),
                control_rx,
                event_tx,
            )
            .await
        });

        while event_rx.recv().await.unwrap() != InstallProgress::WaitingForTransfers {}
        control_tx.send(InstallControl::Cancel).await.unwrap();
        assert!(matches!(
            task.await.unwrap(),
            InstallOutcome::Cancelled {
                staged,
                resume_error: Some(_),
                ..
            } if staged == "verified-stage"
        ));
    }

    #[tokio::test]
    async fn installer_failure_resumes_service_and_reports_the_staged_version() {
        let (mut backend, calls) = fake_backend([Ok(())], Err("atomic swap failed".to_string()));
        let (event_tx, event_rx) = smol::channel::unbounded();
        let (control_tx, control_rx) = smol::channel::unbounded();
        let task = tokio::spawn(async move {
            run_install_flow(
                &mut backend,
                "verified-stage".to_string(),
                "9.9.9".to_string(),
                control_rx,
                event_tx,
            )
            .await
        });
        while event_rx.recv().await.unwrap() != InstallProgress::AwaitingInstallCommit {}
        control_tx.send(InstallControl::Commit).await.unwrap();
        let outcome = task.await.unwrap();

        assert!(matches!(
            outcome,
            InstallOutcome::Failed {
                version,
                message,
                staged: None,
                resume_error: None,
            } if version == "9.9.9" && message == "atomic swap failed"
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            ["bundle", "prepare", "finalize", "install", "cancel"]
        );
        assert_eq!(event_rx.recv().await.unwrap(), InstallProgress::Installing);
    }

    #[tokio::test]
    async fn dropping_ui_while_busy_reopens_the_service() {
        let (mut backend, calls) = fake_backend(
            [Err(crate::service::UpdateQuiescenceError::Busy)],
            Ok(successful_receipt()),
        );
        let (event_tx, event_rx) = smol::channel::unbounded();
        let (control_tx, control_rx) = smol::channel::unbounded();
        let task = tokio::spawn(async move {
            run_install_flow(
                &mut backend,
                "verified-stage".to_string(),
                "9.9.9".to_string(),
                control_rx,
                event_tx,
            )
            .await
        });

        while event_rx.recv().await.unwrap() != InstallProgress::WaitingForTransfers {}
        drop(control_tx);
        let outcome = task.await.unwrap();

        assert!(matches!(
            outcome,
            InstallOutcome::Cancelled {
                resume_error: None,
                ..
            }
        ));
        assert_eq!(*calls.lock().unwrap(), ["bundle", "prepare", "cancel"]);
    }

    #[tokio::test]
    async fn cancel_queued_after_prepare_check_but_before_ui_stop_update_prevents_install() {
        let (mut backend, calls) = fake_backend([Ok(())], Ok(successful_receipt()));
        let (finalize_gate_tx, finalize_gate_rx) = tokio::sync::oneshot::channel();
        let (finalize_entered_tx, finalize_entered_rx) = tokio::sync::oneshot::channel();
        backend.finalize_gate = Some(finalize_gate_rx);
        backend.finalize_entered = Some(finalize_entered_tx);
        let (event_tx, event_rx) = smol::channel::unbounded();
        let (control_tx, control_rx) = smol::channel::unbounded();
        let task = tokio::spawn(async move {
            run_install_flow(
                &mut backend,
                "verified-stage".to_string(),
                "9.9.9".to_string(),
                control_rx,
                event_tx,
            )
            .await
        });

        assert_eq!(
            event_rx.recv().await.unwrap(),
            InstallProgress::CheckingBundle
        );
        assert_eq!(event_rx.recv().await.unwrap(), InstallProgress::Preparing);
        // The stop progress is queued but deliberately not applied to UI state:
        // this reproduces a Cancel click while the view still says Preparing.
        finalize_entered_rx.await.unwrap();
        control_tx.send(InstallControl::Cancel).await.unwrap();
        finalize_gate_tx.send(()).unwrap();

        assert!(matches!(
            task.await.unwrap(),
            InstallOutcome::Cancelled {
                staged,
                resume_error: None,
                ..
            } if staged == "verified-stage"
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            ["bundle", "prepare", "finalize", "cancel"]
        );
        let progress: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        assert!(progress.contains(&InstallProgress::AwaitingInstallCommit));
        assert!(!progress.contains(&InstallProgress::Installing));
    }

    #[tokio::test]
    async fn dropped_confirmation_continuation_after_commit_ack_resumes_without_installing() {
        let (mut backend, calls) = fake_backend([Ok(())], Ok(successful_receipt()));
        let (event_tx, event_rx) = smol::channel::unbounded();
        let (control_tx, control_rx) = smol::channel::unbounded();
        let task = tokio::spawn(async move {
            run_install_flow(
                &mut backend,
                "verified-stage".to_string(),
                "9.9.9".to_string(),
                control_rx,
                event_tx,
            )
            .await
        });

        while event_rx.recv().await.unwrap() != InstallProgress::AwaitingInstallCommit {}
        control_tx.send(InstallControl::Commit).await.unwrap();
        drop(control_tx);

        assert!(matches!(
            task.await.unwrap(),
            InstallOutcome::Cancelled {
                staged,
                resume_error: None,
                ..
            } if staged == "verified-stage"
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            ["bundle", "prepare", "finalize", "cancel"]
        );
        assert!(!std::iter::from_fn(|| event_rx.try_recv().ok())
            .any(|progress| progress == InstallProgress::Installing));
    }
}

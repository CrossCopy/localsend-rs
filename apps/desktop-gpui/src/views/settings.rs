//! Settings: the receiver's identity, port, folder, PIN and the two switches.
//!
//! Network edits are a draft until Apply validates and writes the settings file.
//! Appearance edits preview immediately and Revert restores the last applied
//! values. Network changes restart the receiver; appearance changes update the
//! window directly. Auto-accept is the other exception: the library can flip it
//! on a running server, so it takes effect the moment the switch moves.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState, SliderValue};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Icon, Sizable as _};
use gpui_kit::*;

use crate::app_state::{DesktopApp, InstallState, UpdateState};
use crate::service::{format, ServerStatus, Settings};
use crate::theme;
use crate::theme::s;
use crate::views::{card, chip, section_label};

/// The editable copy of [`Settings`], with one input per text field.
pub struct SettingsForm {
    pub alias: Entity<InputState>,
    pub save_dir: Entity<InputState>,
    pub port: Entity<InputState>,
    pub pin: Entity<InputState>,
    pub surface_opacity: Entity<SliderState>,
    /// Fields the form does not spell in text.
    pub https: bool,
    pub glass_enabled: bool,
    pub auto_accept: bool,
    /// Quick save for starred devices only — the narrow form.
    pub auto_accept_favorites: bool,
    pub error: Option<String>,
    pub saved: bool,
    _subscriptions: Vec<Subscription>,
}

impl SettingsForm {
    pub fn new(settings: &Settings, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let alias = text_input(settings.alias.clone(), "Device name", window, cx);
        let save_dir = text_input(
            settings.save_dir.display().to_string(),
            "~/Downloads/LocalSend",
            window,
            cx,
        );
        let port = text_input(settings.port.to_string(), "53317", window, cx);
        let pin = text_input(
            settings.pin.clone().unwrap_or_default(),
            "123456",
            window,
            cx,
        );
        let surface_opacity = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(f32::from(crate::service::MAX_SURFACE_OPACITY))
                .step(1.0)
                .default_value(f32::from(settings.surface_opacity))
        });
        let opacity_subscription =
            cx.subscribe(&surface_opacity, |form, _, event: &SliderEvent, cx| {
                if matches!(event, SliderEvent::Change(_) | SliderEvent::Release(_)) {
                    form.error = None;
                    form.saved = false;
                    let opacity = if form.glass_enabled {
                        form.surface_opacity.read(cx).value().start().round() as u8
                    } else {
                        crate::service::MAX_SURFACE_OPACITY
                    };
                    theme::apply_surface_opacity(opacity, cx);
                    cx.notify();
                }
            });
        Self {
            alias,
            save_dir,
            port,
            pin,
            surface_opacity,
            https: settings.https,
            glass_enabled: settings.glass_enabled,
            auto_accept: settings.auto_accept,
            auto_accept_favorites: settings.auto_accept_favorites,
            error: None,
            saved: false,
            _subscriptions: vec![opacity_subscription],
        }
    }

    /// The settings as edited. `Err` is the sentence to show under the form.
    pub fn draft(&self, base: &Settings, cx: &App) -> Result<Settings, String> {
        let mut settings = base.clone();
        settings.alias = self.alias.read(cx).value().to_string();
        settings.save_dir = std::path::PathBuf::from(self.save_dir.read(cx).value().to_string());
        settings.port = self
            .port
            .read(cx)
            .value()
            .trim()
            .parse::<u16>()
            .map_err(|_| "The port must be a number between 1 and 65535.".to_string())?;
        settings.pin = Some(self.pin.read(cx).value().to_string());
        settings.https = self.https;
        settings.glass_enabled = self.glass_enabled;
        let opacity = self.surface_opacity.read(cx).value();
        let SliderValue::Single(opacity) = opacity else {
            return Err("Surface opacity must be a single slider value.".to_string());
        };
        settings.surface_opacity = opacity.round().clamp(0.0, 100.0) as u8;
        settings.auto_accept = self.auto_accept;
        settings.auto_accept_favorites = self.auto_accept_favorites;
        let settings = settings.normalized();
        settings.validate()?;
        Ok(settings)
    }

    /// Reflect settings that arrived from somewhere other than this form.
    pub fn reload(&mut self, settings: &Settings, window: &mut Window, cx: &mut Context<Self>) {
        self.alias.update(cx, |state, cx| {
            state.set_value(settings.alias.clone(), window, cx)
        });
        self.save_dir.update(cx, |state, cx| {
            state.set_value(settings.save_dir.display().to_string(), window, cx)
        });
        self.port.update(cx, |state, cx| {
            state.set_value(settings.port.to_string(), window, cx)
        });
        self.pin.update(cx, |state, cx| {
            state.set_value(settings.pin.clone().unwrap_or_default(), window, cx)
        });
        self.surface_opacity.update(cx, |state, cx| {
            state.set_value(f32::from(settings.surface_opacity), window, cx)
        });
        self.https = settings.https;
        self.glass_enabled = settings.glass_enabled;
        self.auto_accept = settings.auto_accept;
        self.auto_accept_favorites = settings.auto_accept_favorites;
        self.error = None;
        self.saved = false;
    }

    /// Push one text field's value in from outside (folder picker).
    pub fn set_save_dir(
        &mut self,
        dir: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_dir.update(cx, |state, cx| {
            state.set_value(dir.display().to_string(), window, cx)
        });
    }
}

fn text_input(
    value: String,
    placeholder: &'static str,
    window: &mut Window,
    cx: &mut Context<SettingsForm>,
) -> Entity<InputState> {
    let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
    input.update(cx, |state, cx| state.set_value(value, window, cx));
    input
}

pub fn render(app: &DesktopApp, cx: &mut Context<DesktopApp>) -> AnyElement {
    // Snapshot the form before building elements: `cx.listener` needs a mutable
    // context, which cannot coexist with the immutable borrow `read` holds.
    let form = app.settings_form.read(cx);
    let alias = form.alias.clone();
    let save_dir = form.save_dir.clone();
    let port = form.port.clone();
    let pin = form.pin.clone();
    let surface_opacity = form.surface_opacity.clone();
    let https = form.https;
    let glass_enabled = form.glass_enabled;
    let auto_accept = form.auto_accept;
    let auto_accept_favorites = form.auto_accept_favorites;
    let error = form.error.clone();
    let saved = form.saved;
    let status = app.store.read(cx).session().status.clone();
    let fingerprint = match &status {
        ServerStatus::Listening { fingerprint, .. } => Some(fingerprint.clone()),
        _ => None,
    };
    let listening = status.is_listening();

    // One measured column, centred in the pane. A settings form pinned to the
    // left edge leaves half the window blank, and a row that spans the whole
    // width makes a two-word description look like a paragraph.
    let mut column = v_flex()
        .w_full()
        .max_w(s(600.))
        .gap(s(14.))
        .child(
            div()
                .text_size(s(12.))
                .text_color(cx.theme().muted_foreground)
                .child("Network changes restart the receiver; appearance changes apply without a restart."),
        )
        .child(
            card(cx)
                .gap(s(16.))
                .child(section_label("This device", cx))
                .child(field(
                    "Device name",
                    "What other devices show in their list.",
                    Input::new(&alias).aria_label("Device name"),
                    cx,
                ))
                .child(field(
                    "Receiving folder",
                    "Where accepted files are written.",
                    h_flex()
                        .w_full()
                        .gap(s(8.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&save_dir).aria_label("Receiving folder")),
                        )
                        .child(
                            crate::theme::icon_button(
                                "choose-save-dir",
                                Icon::new(IconName::FolderOpen),
                                cx,
                            )
                            .accessibility_label("Choose a folder")
                            .tooltip("Choose a folder")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.pick_save_dir(window, cx)),
                            ),
                        ),
                    cx,
                ))
                .child(field(
                    "Port",
                    "53317 is the LocalSend default. Change it only if something else holds the port.",
                    Input::new(&port).aria_label("Port"),
                    cx,
                ))
                .child(field(
                    "PIN",
                    "4–12 digits. Senders must type it before anything is accepted; leave empty for an open receiver.",
                    Input::new(&pin).aria_label("PIN"),
                    cx,
                )),
        )
        .child(
            card(cx)
                .gap(s(16.))
                .child(section_label("Transport", cx))
                .child(switch_row(
                    Switch::new("https-switch")
                        .checked(https)
                        .accessibility_label("Use HTTPS"),
                    "Use HTTPS",
                    "HTTPS is what the official app uses. Turning it off makes this device talkable-to by older tools, and its fingerprint stops being derived from a certificate.",
                    cx,
                    cx.listener(|this, checked: &bool, _, cx| {
                        this.settings_form.update(cx, |form, _| {
                            form.https = *checked;
                            form.saved = false;
                        });
                        cx.notify();
                    }),
                ))
                .child(switch_row(
                    Switch::new("auto-accept-switch")
                        .checked(auto_accept)
                        .accessibility_label("Accept without asking"),
                    "Accept without asking",
                    "Every offer is taken straight away, into the receiving folder. Useful for a drop box; risky on a shared network.",
                    cx,
                    cx.listener(|this, checked: &bool, _, cx| {
                        this.settings_form.update(cx, |form, _| {
                            form.auto_accept = *checked;
                            form.saved = false;
                        });
                        // Live flip: the receiver reads this per request, so it works
                        // without waiting for Apply.
                        this.store.read(cx).set_auto_accept(*checked);
                        cx.notify();
                    }),
                ))
                .child(switch_row(
                    Switch::new("auto-accept-favorites-switch")
                        .checked(auto_accept_favorites)
                        .accessibility_label("Accept without asking from favorites"),
                    "Accept without asking from favorites",
                    "The narrow version: only devices you starred on the Send screen are taken silently. A stranger still gets a prompt.",
                    cx,
                    cx.listener(|this, checked: &bool, _, cx| {
                        this.settings_form.update(cx, |form, _| {
                            form.auto_accept_favorites = *checked;
                            form.saved = false;
                        });
                        cx.notify();
                    }),
                )),
        )
        .child(
            card(cx)
                .gap(s(16.))
                .child(section_label("Appearance", cx))
                .child(switch_row(
                    Switch::new("glass-switch")
                        .checked(glass_enabled)
                        .accessibility_label("Use glass window"),
                    "Use glass window",
                    "Show the macOS blurred material behind the app's surfaces.",
                    cx,
                    cx.listener(|this, checked: &bool, window, cx| {
                        this.settings_form.update(cx, |form, _| {
                            form.glass_enabled = *checked;
                            form.error = None;
                            form.saved = false;
                        });
                        let opacity = if *checked {
                            this.settings_form
                                .read(cx)
                                .surface_opacity
                                .read(cx)
                                .value()
                                .start()
                                .round() as u8
                        } else {
                            crate::service::MAX_SURFACE_OPACITY
                        };
                        theme::apply_surface_opacity(opacity, cx);
                        window.set_background_appearance(if *checked {
                            WindowBackgroundAppearance::Blurred
                        } else {
                            WindowBackgroundAppearance::Opaque
                        });
                        cx.notify();
                    }),
                ))
                .child(field(
                    "Surface opacity",
                    "Lower opacity shows more of the blurred desktop; 100% is fully opaque.",
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap(s(12.))
                        .child(
                            div()
                                .id("surface-opacity-slider")
                                .flex_1()
                                .h(s(28.))
                                .child(Slider::new(&surface_opacity).disabled(!glass_enabled)),
                        )
                        .child(
                            div()
                                .w(s(42.))
                                .text_right()
                                .text_size(s(11.))
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "{:.0}%",
                                    surface_opacity.read(cx).value().start()
                                )),
                        ),
                    cx,
                ))
                .child(
                    div()
                        .text_size(s(10.5))
                        .text_color(cx.theme().muted_foreground)
                        .child("Blur strength is controlled by macOS; GPUI does not expose a radius setting."),
                ),
        )
        .child(
            card(cx)
                .gap(s(10.))
                .child(section_label("Identity", cx))
                .child(
                    div()
                        .text_size(s(11.5))
                        .text_color(cx.theme().muted_foreground)
                        .child(if listening {
                            "This fingerprint is what other devices pin when they talk to this one over HTTPS."
                        } else {
                            "The fingerprint appears here once the receiver is listening."
                        }),
                )
        .children(fingerprint.map(|fingerprint| {
            h_flex()
                .gap(s(8.))
                        .child(chip(
                            format::short_fingerprint(&fingerprint),
                            cx.theme().primary,
                        ))
                        .child(
                            div()
                                .text_size(s(10.5))
                                .text_color(cx.theme().muted_foreground)
                                .child("SHA-256 of the served certificate"),
                )
        })),
    );

    let update_card = card(cx)
        .gap(s(12.))
        .child(section_label("Software updates", cx))
        .child(
            div()
                .text_size(s(11.5))
                .text_color(cx.theme().muted_foreground)
                .child(format!("Installed version: {}", env!("CARGO_PKG_VERSION"))),
        )
        .child(
            div()
                .id("updates-signing-trust")
                .test_support()
                .aria_label(crate::app_state::update_signing_trust_message())
                .text_size(s(11.))
                .text_color(if crate::app_state::UPDATE_SIGNING_TRUST_CONFIGURED {
                    cx.theme().muted_foreground
                } else {
                    theme::warning(cx)
                })
                .child(crate::app_state::update_signing_trust_message()),
        );
    let update_card = match &app.update_state {
        UpdateState::Idle => update_card
            .child(update_status("Check GitHub for a newer stable version.".to_owned()))
            .child(
                Button::new("updates-check")
                    .small()
                    .label("Check for updates")
                    .on_click(cx.listener(|this, _, _, cx| this.check_for_updates(cx))),
            ),
        UpdateState::Checking => update_card
            .child(update_status("Checking for updates…".to_owned())),
        UpdateState::UpToDate => update_card
            .child(update_status("You're up to date.".to_owned()))
            .child(
                Button::new("updates-check")
                    .ghost()
                    .small()
                    .label("Check again")
                    .on_click(cx.listener(|this, _, _, cx| this.check_for_updates(cx))),
            ),
        UpdateState::Available(release) => {
            let card = update_card
                .child(update_status(format!(
                    "Version {} is available.",
                    release.version()
                )))
                .child(release_notes(release.notes(), cx));
            if crate::app_state::UPDATE_SIGNING_TRUST_CONFIGURED {
                card.child(
                    Button::new("updates-download-request")
                        .primary()
                        .small()
                        .label("Download and verify…")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.request_update_download(cx)
                        })),
                )
            } else {
                card.child(
                    div()
                        .id("updates-stage-unavailable")
                        .test_support()
                        .aria_label(crate::app_state::update_signing_trust_message())
                        .text_size(s(11.))
                        .text_color(theme::warning(cx))
                        .child(crate::app_state::update_signing_trust_message()),
                )
            }
        }
        UpdateState::ConfirmingDownload(release) => update_card
            .child(update_status(format!(
                "Download and verify version {}? This downloads the release and checks its integrity and publisher signature.",
                release.version()
            )))
            .child(release_notes(release.notes(), cx))
            .child(
                h_flex()
                    .gap(s(8.))
                    .child(
                        Button::new("updates-download-cancel")
                            .ghost()
                            .small()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.cancel_update_download(cx)
                            })),
                    )
                    .child(
                        Button::new("updates-download-confirm")
                            .primary()
                            .small()
                            .label("Download and verify")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.confirm_update_download(cx)
                            })),
                    ),
            ),
        UpdateState::Staging(release) => update_card.child(
            update_status(format!(
                "Downloading and verifying version {}…",
                release.version()
            )),
        ),
        UpdateState::Staged { release, update } => update_card
            .child(update_status(format!(
                "Version {} is downloaded and verified.",
                update.version()
            )))
            .child(release_notes(release.notes(), cx))
            .child(
                div()
                    .text_size(s(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child("Installation targets only the exact running valid .app; standalone builds cannot install. The verified bundle remains staged."),
            )
            .child(install_actions(
                &update.version().to_string(),
                &app.install_state,
                cx,
            )),
        UpdateState::Installing(release) => update_card
            .child(update_status(format!(
                "Version {} is ready for installation.",
                release.version()
            )))
            .child(release_notes(release.notes(), cx))
            .child(install_actions(
                &release.version().to_string(),
                &app.install_state,
                cx,
            )),
        UpdateState::Failed(message) => update_card
            .child(
                div()
                    .text_color(theme::danger(cx))
                    .child(update_status(message.clone())),
            )
            .children(install_failure_details(&app.install_state, cx))
            .child(
                Button::new("updates-check")
                    .ghost()
                    .small()
                    .label("Try again")
                    .on_click(cx.listener(|this, _, _, cx| this.check_for_updates(cx))),
            ),
    };
    column = column.child(update_card);

    if let Some(error) = &error {
        column = column.child(
            h_flex()
                .gap(s(7.))
                .child(
                    Icon::new(IconName::CircleAlert)
                        .size(s(14.))
                        .text_color(theme::danger(cx)),
                )
                .child(
                    div()
                        .text_size(s(12.))
                        .text_color(theme::danger(cx))
                        .child(error.clone()),
                ),
        );
    }
    if saved {
        column = column.child(
            h_flex()
                .gap(s(7.))
                .child(
                    Icon::new(IconName::CircleCheck)
                        .size(s(14.))
                        .text_color(theme::success(cx)),
                )
                .child(
                    div()
                        .text_size(s(12.))
                        .text_color(theme::success(cx))
                        .child("Saved. Settings are applied."),
                ),
        );
    }

    column = column.child(
        h_flex()
            .w_full()
            .justify_center()
            .gap(s(8.))
            .child(
                Button::new("settings-reset")
                    .ghost()
                    .small()
                    .label("Revert")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.revert_settings(window, cx);
                    })),
            )
            .child(
                Button::new("settings-apply")
                    .primary()
                    .small()
                    .label("Apply")
                    .on_click(cx.listener(|this, _, window, cx| this.apply_settings(window, cx))),
            ),
    );

    v_flex()
        .id("settings-scroll")
        .flex_1()
        .min_h_0()
        .w_full()
        .overflow_y_scroll()
        .p(s(20.))
        .items_center()
        .child(column)
        .into_any_element()
}

fn update_status(message: String) -> impl IntoElement {
    div()
        .id("updates-status")
        .test_support()
        .aria_label(message.clone())
        .text_size(s(12.))
        .child(message)
}

fn install_actions(version: &str, state: &InstallState, cx: &mut Context<DesktopApp>) -> Div {
    let actions = match state {
        InstallState::Idle | InstallState::Failed { .. } => {
            let mut actions = v_flex().gap(s(8.));
            if let InstallState::Failed {
                version,
                message,
                resume_error,
            } = state
            {
                actions = actions
                    .child(
                        div()
                            .text_size(s(11.))
                            .text_color(theme::danger(cx))
                            .child(format!("Install of {version} failed: {message}")),
                    )
                    .children(resume_error.as_ref().map(|message| {
                        div()
                            .text_size(s(11.))
                            .text_color(theme::danger(cx))
                            .child(message.clone())
                    }));
            }
            actions.child(
                Button::new("updates-install-request")
                    .primary()
                    .small()
                    .label("Install and relaunch…")
                    .on_click(cx.listener(|this, _, _, cx| this.request_update_install(cx))),
            )
        }
        InstallState::Confirming => v_flex()
            .gap(s(8.))
            .child(
                div()
                    .text_size(s(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .child(format!("Confirm installation of version {version}?")),
            )
            .child(
                div()
                    .text_size(s(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child("Incoming and outgoing admission will close while current transfers drain. Pending offers may be declined. The listener will stop before replacement; if installation fails, the receiver will be resumed. Relaunch being requested does not verify that startup succeeds."),
            )
            .child(
                h_flex()
                    .gap(s(8.))
                    .child(
                        Button::new("updates-install-cancel")
                            .ghost()
                            .small()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.cancel_update_install_confirmation(cx)
                            })),
                    )
                    .child(
                        Button::new("updates-install-confirm")
                            .primary()
                            .small()
                            .label("Confirm install")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.confirm_update_install(cx)
                            })),
                    ),
            ),
        InstallState::Preparing => v_flex()
            .gap(s(8.))
            .child(update_status("Checking whether transfers can safely drain…".to_owned()))
            .child(
                Button::new("updates-install-cancel")
                    .ghost()
                    .small()
                    .label("Cancel")
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_update_install(cx))),
            ),
        InstallState::WaitingForTransfers => v_flex()
            .gap(s(8.))
            .child(update_status(
                "Waiting for current transfers. New transfers are paused.".to_owned(),
            ))
            .child(
                h_flex()
                    .gap(s(8.))
                    .child(
                        Button::new("updates-install-retry")
                            .small()
                            .label("Check again")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.retry_update_install(cx)
                            })),
                    )
                    .child(
                        Button::new("updates-install-cancel")
                            .ghost()
                            .small()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_update_install(cx))),
                    ),
            ),
        InstallState::Resuming => v_flex()
            .gap(s(8.))
            .child(update_status(
                "Cancelling the update and reopening the receiver…".to_owned(),
            )),
        InstallState::StoppingListener => v_flex()
            .gap(s(8.))
            .child(update_status(
                "Stopping the receiver before replacing the app…".to_owned(),
            ))
            .child(
                Button::new("updates-install-cancel")
                    .ghost()
                    .small()
                    .label("Cancel")
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_update_install(cx))),
            ),
        InstallState::Installing => v_flex()
            .gap(s(8.))
            .child(update_status(format!(
                "Installing version {version}… Cancellation is no longer available."
            ))),
        InstallState::RelaunchRequested {
            version,
            previous_bundle,
        } => v_flex()
            .gap(s(8.))
            .child(update_status(format!(
                "Relaunch of version {version} was requested; startup is not verified."
            )))
            .child(
                div()
                    .text_size(s(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "Previous app bundle retained at {}",
                        previous_bundle.display()
                    )),
            ),
    };
    actions.w_full()
}

fn install_failure_details(state: &InstallState, cx: &App) -> Option<Div> {
    let InstallState::Failed {
        version,
        message,
        resume_error,
    } = state
    else {
        return None;
    };
    Some(
        v_flex()
            .gap(s(5.))
            .child(
                div()
                    .text_size(s(11.))
                    .text_color(theme::danger(cx))
                    .child(format!("Installation of {version} failed: {message}")),
            )
            .children(resume_error.as_ref().map(|message| {
                div()
                    .text_size(s(11.))
                    .text_color(theme::danger(cx))
                    .child(message.clone())
            })),
    )
}

fn release_notes(notes: Option<&str>, cx: &App) -> Div {
    v_flex()
        .w_full()
        .gap(s(4.))
        .child(
            div()
                .text_size(s(11.))
                .font_weight(FontWeight::MEDIUM)
                .child("Release notes"),
        )
        .child(
            div()
                .text_size(s(11.))
                .text_color(cx.theme().muted_foreground)
                .child(
                    notes
                        .unwrap_or("No release notes were provided.")
                        .to_owned(),
                ),
        )
}

/// A labelled field: label, control, explanation — stacked.
///
/// A side-by-side label gutter forces a 140 pt column of mostly-empty space and
/// wraps the explanation under the control at a ragged left edge. Stacked, the
/// label names the control directly and the description sits on the same left
/// edge as both.
fn field(
    label: &'static str,
    description: &'static str,
    control: impl IntoElement,
    cx: &App,
) -> Div {
    v_flex()
        .w_full()
        .gap(s(6.))
        .child(
            div()
                .text_size(s(12.5))
                .font_weight(FontWeight::MEDIUM)
                .child(label),
        )
        .child(control)
        .child(
            div()
                .text_size(s(10.5))
                .text_color(cx.theme().muted_foreground)
                .child(description),
        )
}

/// A switch with its name and explanation on the left, the control on the right
/// where every other settings window on the platform puts it.
fn switch_row(
    switch: Switch,
    label: &'static str,
    description: &'static str,
    cx: &App,
    on_change: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> Div {
    h_flex()
        .w_full()
        .items_start()
        .gap(s(16.))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(s(3.))
                .child(
                    div()
                        .text_size(s(12.5))
                        .font_weight(FontWeight::MEDIUM)
                        .child(label),
                )
                .child(
                    div()
                        .text_size(s(10.5))
                        .text_color(cx.theme().muted_foreground)
                        .child(description),
                ),
        )
        .child(switch.on_click(on_change))
}
